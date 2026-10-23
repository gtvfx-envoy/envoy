//! Runtime stack loading and context-aware resolution.
//!
//! A stack is a strict YAML document with an `.estack` extension. Its filename
//! supplies its name, and its document defines an ordered collection of envoy
//! bundles that together form an isolated runtime environment. Named stacks
//! may be published to versioned directories under
//! [`ENVOY_STACK_ROOTS`](crate::stack_registry::STACK_ROOTS_VAR).
//!
//! A stack that has been resolved with a cache root is remembered there (see
//! [`stack_snapshot`](crate::stack_snapshot)), so that it can start again from
//! the bundles in the local store when its share cannot be reached.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::bundle_store::{cached_copy, Store};
use crate::discovery::{
    expand_bundle_path, resolve_input_path, validate_bundle, Bundle, BundleInfo, BUNDLE_CHECKOUT,
    BUNDLE_ENV_DIR,
};
use crate::error::{EnvoyError, Result};
use crate::stack_registry::{
    list_named_stacks, lookup_named_stack, published_version, resolve_named_stack, NamedStackEntry,
    NamedStackLookup,
};
use crate::stack_snapshot::{self, Selector, Snapshot, SnapshotBundle, SnapshotInfo};
use crate::user_config::UserConfig;

/// Environment variable selecting a named stack or `.estack` path.
pub const STACK_VAR: &str = "ENVOY_STACK";

/// Environment variable selecting a colon-separated stack context.
pub const STACK_CONTEXT_VAR: &str = "ENVOY_STACK_CONTEXT";

/// User-config key selecting a named stack or `.estack` path.
pub const STACK_SETTING: &str = "stack";

/// Default namespace used after context hierarchy matching is exhausted.
pub const DEFAULT_STACK_NAMESPACE: &str = "gt";

/// Default maximum number of context levels considered during resolution.
pub const DEFAULT_STACK_MAX_DEPTH: usize = 16;

/// Source of a loaded stack definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StackSource {
    /// Stack loaded from a local `.estack` file.
    Local { path: PathBuf },
}

/// One bundle declaration in a stack.
#[derive(Clone, Debug)]
pub struct StackBundleEntry {
    /// Resolved bundle.
    pub bundle: Bundle,
    /// Arbitrary stack-local metadata attached to the bundle entry.
    pub metadata: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackDocument {
    #[serde(default = "default_namespace")]
    namespace: String,
    #[serde(default, rename = "source")]
    _source: StackSourceDocument,
    #[serde(default)]
    pinned_version: Option<String>,
    #[serde(default)]
    metadata: HashMap<String, Value>,
    bundles: Vec<StackBundleDocument>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum StackSourceDocument {
    #[default]
    Local,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackBundleDocument {
    path: String,
    #[serde(default)]
    metadata: HashMap<String, Value>,
}

fn default_namespace() -> String {
    DEFAULT_STACK_NAMESPACE.to_string()
}

/// A loaded runtime stack and its ordered bundle collection.
pub struct Stack {
    path: PathBuf,
    name: String,
    namespace: String,
    source: StackSource,
    pinned_version: Option<String>,
    registry_version: Option<String>,
    metadata: HashMap<String, Value>,
    bundle_documents: Vec<StackBundleDocument>,
    bundle_entries: RefCell<Option<Vec<StackBundleEntry>>>,
    selector: Option<Selector>,
    snapshot: Option<SnapshotInfo>,
}

/// Why a stack could not be read from where it lives.
enum Failure {
    /// Where the stack lives could not be reached, for this reason. A snapshot
    /// of the stack may stand in for it.
    Unreachable { reason: String, error: EnvoyError },
    /// The stack is wrong, or it is not there to be found. A snapshot must not
    /// hide that.
    Broken(EnvoyError),
}

impl Failure {
    fn into_error(self) -> EnvoyError {
        match self {
            Self::Unreachable { error, .. } | Self::Broken(error) => error,
        }
    }
}

impl Stack {
    /// Load and validate a stack from a filesystem path.
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        Self::load(path.as_ref(), None, None)
    }

    /// Resolve and load the latest version of a named stack.
    pub fn from_name(name: &str) -> Result<Self> {
        let path = resolve_named_stack(name).ok_or_else(|| {
            EnvoyError::Validation(format!(
                "Named stack {name:?} not found in ENVOY_STACK_ROOTS"
            ))
        })?;

        Self::load(&path, Some(name), Some(&published_version(&path)))
    }

    /// Resolve a stack the way `--stack` does: `raw` is a published name or the
    /// path of an `.estack` file.
    ///
    /// With a `cache_root`, the result is remembered there, and when the stack
    /// cannot be reached later, because its share is down, it is rebuilt from
    /// that memory using the bundles in the local store. Check
    /// [`snapshot_info`](Self::snapshot_info) to tell. A stack that is broken,
    /// or a name that is not published on a share that can be reached, is an
    /// error as ever. Without a `cache_root` this is [`Stack::new`] or
    /// [`Stack::from_name`].
    ///
    /// Nothing is remembered until [`save_snapshot`](Self::save_snapshot).
    pub fn from_selector(raw: &str, cache_root: Option<&Path>) -> Result<Self> {
        let selector = Selector::parse(raw);
        match Self::open_selected(&selector) {
            Ok(mut stack) => {
                stack.selector = Some(selector);
                Ok(stack)
            }
            Err(Failure::Broken(error)) => Err(error),
            Err(Failure::Unreachable { reason, error }) => match cache_root {
                Some(root) => Self::from_snapshot(raw, selector, root, reason, error),
                // Nothing to fall back on, but the real cause is worth saying.
                None => Err(match error {
                    EnvoyError::Validation(message) => {
                        EnvoyError::Validation(format!("{message} ({reason})"))
                    }
                    other => other,
                }),
            },
        }
    }

    fn open_selected(selector: &Selector) -> std::result::Result<Self, Failure> {
        match selector {
            Selector::Path(path) => Self::open(path, None, None),
            Selector::Name(name) => match lookup_named_stack(name) {
                NamedStackLookup::Found(path) => {
                    Self::open(&path, Some(name), Some(&published_version(&path)))
                }
                NamedStackLookup::Missing { unreachable } => {
                    let error = EnvoyError::Validation(format!(
                        "Named stack {name:?} not found in ENVOY_STACK_ROOTS"
                    ));
                    Err(if unreachable.is_empty() {
                        Failure::Broken(error)
                    } else {
                        let roots = unreachable
                            .iter()
                            .map(|root| root.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let plural = if unreachable.len() == 1 { "" } else { "s" };
                        Failure::Unreachable {
                            reason: format!("the stack root{plural} {roots} cannot be reached"),
                            error,
                        }
                    })
                }
            },
        }
    }

    /// Rebuild a stack from its snapshot, with the bundles the local store has.
    fn from_snapshot(
        raw: &str,
        selector: Selector,
        cache_root: &Path,
        reason: String,
        error: EnvoyError,
    ) -> Result<Self> {
        let Some((snapshot, confirmed)) = stack_snapshot::load(cache_root, &selector) else {
            // Say what was asked for and why it failed, as the error always did.
            let what = match error {
                EnvoyError::Validation(message) => format!("{message} ({reason})"),
                _ => reason,
            };
            return Err(EnvoyError::Validation(format!(
                "{what}, and no earlier run of it is remembered on this machine"
            )));
        };

        let store = Store::open_existing(cache_root).ok().flatten();
        let mut entries = Vec::new();
        let mut unavailable = Vec::new();
        for saved in &snapshot.bundles {
            match offline_bundle(saved, store.as_ref()) {
                Some(bundle) => entries.push(StackBundleEntry {
                    bundle,
                    metadata: saved.metadata.clone(),
                }),
                None => unavailable.push(saved.label()),
            }
        }
        if entries.is_empty() {
            return Err(EnvoyError::Validation(format!(
                "Stack {raw:?} is not available: {reason}, and none of its bundles are cached on \
this machine ({})",
                unavailable.join(", ")
            )));
        }

        Ok(Self {
            path: snapshot.path.clone(),
            name: snapshot.name,
            namespace: snapshot.namespace,
            source: StackSource::Local {
                path: snapshot.path,
            },
            pinned_version: snapshot.pinned_version,
            registry_version: snapshot.registry_version,
            metadata: snapshot.metadata,
            bundle_documents: Vec::new(),
            bundle_entries: RefCell::new(Some(entries)),
            selector: Some(selector),
            snapshot: Some(SnapshotInfo {
                confirmed,
                reason,
                unavailable,
            }),
        })
    }

    /// Remember this stack under `cache_root`, as [`from_selector`](Self::from_selector)
    /// uses it, when it is new or has changed. Returns whether anything was
    /// written.
    ///
    /// "Changed" means the stack file, or where one of its bundles is, what it
    /// is called, or the version its marker states: a bundle republished in
    /// place at the same path is a change. A bundle whose marker exists but
    /// cannot be read keeps what was recorded for it, so a share that is
    /// failing never replaces a good record with a worse one.
    ///
    /// Call it once the stack's bundles have loaded, so that only a stack that
    /// worked is kept. A stack that was not found with
    /// [`from_selector`](Self::from_selector), or that was itself rebuilt from
    /// a snapshot, has nothing to remember, and neither has a stack with no
    /// published bundle: only a published bundle can be found in the store, so
    /// a stack of checkouts has nothing to start from, and the cache is left
    /// alone.
    pub fn save_snapshot(&self, cache_root: &Path) -> Result<bool> {
        let (Some(selector), None) = (self.selector.as_ref(), self.snapshot.as_ref()) else {
            return Ok(false);
        };
        let entries = self.bundle_entries()?;
        let saved = stack_snapshot::load(cache_root, selector).map(|(snapshot, _)| snapshot);

        let bundles: Vec<SnapshotBundle> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let recorded = saved.as_ref().and_then(|saved| saved.bundles.get(index));
                snapshot_bundle(entry, recorded)
            })
            .collect();
        if bundles.iter().all(|bundle| bundle.version.is_none()) {
            return Ok(false);
        }

        let stamp = stack_snapshot::stamp_of(&self.path);
        if let Some(saved) = &saved {
            if saved.bundles == bundles && saved.is_of(&self.path, stamp) {
                stack_snapshot::confirm(cache_root, selector);
                return Ok(false);
            }
        }

        let mut snapshot = Snapshot::new(selector, self.path.clone(), stamp);
        snapshot.name = self.name.clone();
        snapshot.namespace = self.namespace.clone();
        snapshot.registry_version = self.registry_version.clone();
        snapshot.pinned_version = self.pinned_version.clone();
        snapshot.metadata = self.metadata.clone();
        snapshot.bundles = bundles;

        stack_snapshot::save(cache_root, selector, &snapshot).map_err(|source| EnvoyError::Io {
            path: stack_snapshot::folder(cache_root),
            source,
        })?;
        Ok(true)
    }

    /// How this stack was rebuilt from its snapshot, or `None` when it was
    /// read from where it lives.
    pub fn snapshot_info(&self) -> Option<&SnapshotInfo> {
        self.snapshot.as_ref()
    }

    /// Return the directly configured or context-resolved current stack.
    ///
    /// Resolution precedence is `ENVOY_STACK`, the `stack` user setting, and
    /// then `ENVOY_STACK_CONTEXT`. No registry lookup occurs without a context.
    pub fn current(
        ignore_user_config: bool,
        context: Option<&str>,
        default_namespace: &str,
        max_depth: usize,
    ) -> Result<Option<Self>> {
        Self::current_with_snapshots(
            ignore_user_config,
            context,
            default_namespace,
            max_depth,
            None,
        )
    }

    /// [`Stack::current`] for a caller that is about to run something, so that
    /// a selected stack can be rebuilt from its snapshot under `cache_root`
    /// when it cannot be reached (see [`Stack::from_selector`]). A stack found
    /// through a context is always read from the registry.
    pub fn current_with_snapshots(
        ignore_user_config: bool,
        context: Option<&str>,
        default_namespace: &str,
        max_depth: usize,
        cache_root: Option<&Path>,
    ) -> Result<Option<Self>> {
        if let Some(raw) = non_empty_env(STACK_VAR) {
            return Self::from_selector(&raw, cache_root).map(Some);
        }

        if !ignore_user_config {
            let user_config = UserConfig::load(None);
            if let Some(raw) = user_config.get(STACK_SETTING) {
                return Self::from_selector(raw, cache_root).map(Some);
            }
        }

        let context = context
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| non_empty_env(STACK_CONTEXT_VAR));

        context
            .as_deref()
            .map(|value| Self::resolve_optional(value, default_namespace, max_depth))
            .transpose()
            .map(Option::flatten)
    }

    /// Resolve a named stack for a colon-separated context hierarchy.
    pub fn resolve(context: &str, default_namespace: &str, max_depth: usize) -> Result<Self> {
        Self::resolve_optional(context, default_namespace, max_depth)?.ok_or_else(|| {
            EnvoyError::Validation(format!(
                "No stack found for context {context:?} or default namespace {default_namespace:?}"
            ))
        })
    }

    fn resolve_optional(
        context: &str,
        default_namespace: &str,
        max_depth: usize,
    ) -> Result<Option<Self>> {
        let context = StackContext::new(context)?;
        let entries = load_registry_stacks()?;

        for level in context.most_specific_levels(max_depth) {
            if let Some(stack) = unique_namespace_match(&entries, &level)? {
                return Ok(Some(stack));
            }
        }

        if let Some(stack) = unique_namespace_match(&entries, default_namespace)? {
            return Ok(Some(stack));
        }

        Ok(None)
    }

    fn from_named_entry(entry: &NamedStackEntry) -> Result<Self> {
        Self::load(
            &entry.path,
            Some(entry.name.as_str()),
            Some(entry.version.as_str()),
        )
    }

    fn load(
        path: &Path,
        expected_name: Option<&str>,
        registry_version: Option<&str>,
    ) -> Result<Self> {
        Self::open(path, expected_name, registry_version).map_err(Failure::into_error)
    }

    fn open(
        path: &Path,
        expected_name: Option<&str>,
        registry_version: Option<&str>,
    ) -> std::result::Result<Self, Failure> {
        let path = resolve_input_path(path);
        validate_stack_extension(&path).map_err(Failure::Broken)?;
        if !path.is_file() {
            let error =
                EnvoyError::Validation(format!("Stack path does not exist: {}", path.display()));
            // A file missing from a folder that is there was taken away; one
            // missing from a folder that is not there may be on a share that is down.
            return Err(match path.parent().filter(|folder| !folder.is_dir()) {
                Some(folder) => Failure::Unreachable {
                    reason: format!("the folder {} cannot be reached", folder.display()),
                    error,
                },
                None => Failure::Broken(error),
            });
        }

        let contents = fs::read_to_string(&path).map_err(|source| read_failure(&path, source))?;
        let document: StackDocument = serde_yaml::from_str(&contents).map_err(|source| {
            Failure::Broken(EnvoyError::Validation(format!(
                "Invalid YAML stack at {}: {source}",
                path.display()
            )))
        })?;

        let name = stack_name_from_path(&path).map_err(Failure::Broken)?;
        validate_document(&path, &document).map_err(Failure::Broken)?;
        if let Some(expected_name) = expected_name {
            if name != expected_name {
                return Err(Failure::Broken(EnvoyError::Validation(format!(
                    "Stack filename {name:?} does not match registry slot {expected_name:?}"
                ))));
            }
        }
        validate_bundle_documents(&path, &document.bundles).map_err(Failure::Broken)?;

        Ok(Self {
            path: path.clone(),
            name,
            namespace: document.namespace,
            source: StackSource::Local { path },
            pinned_version: document.pinned_version,
            registry_version: registry_version.map(ToOwned::to_owned),
            metadata: document.metadata,
            bundle_documents: document.bundles,
            bundle_entries: RefCell::new(None),
            selector: None,
            snapshot: None,
        })
    }

    /// Return the absolute stack-file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the stack name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the context namespace matched by this stack.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Return where the stack definition was loaded from.
    pub fn source(&self) -> &StackSource {
        &self.source
    }

    /// Return the optional informational pinned version.
    pub fn pinned_version(&self) -> Option<&str> {
        self.pinned_version.as_deref()
    }

    /// Return the named-registry publication version, if applicable.
    pub fn registry_version(&self) -> Option<&str> {
        self.registry_version.as_deref()
    }

    /// Return stack-level metadata.
    pub fn metadata(&self) -> &HashMap<String, Value> {
        &self.metadata
    }

    /// Return resolved entries, including stack-local bundle metadata.
    pub fn bundle_entries(&self) -> Result<Vec<StackBundleEntry>> {
        if let Some(entries) = self.bundle_entries.borrow().as_ref() {
            return Ok(entries.clone());
        }

        let entries = self
            .bundle_documents
            .iter()
            .map(|document| {
                let path = resolve_bundle_path(&self.path, &document.path)?;
                let bundle = Bundle::new(&path, None)?;
                Ok(StackBundleEntry {
                    bundle,
                    metadata: document.metadata.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        *self.bundle_entries.borrow_mut() = Some(entries.clone());
        Ok(entries)
    }

    /// Return the ordered bundles declared by this stack.
    pub fn bundles(&self) -> Result<Vec<Bundle>> {
        self.bundle_entries()
            .map(|entries| entries.into_iter().map(|entry| entry.bundle).collect())
    }

    /// Return bundle discovery records for command/runtime loading.
    pub fn bundle_infos(&self) -> Result<Vec<BundleInfo>> {
        self.bundles().map(|bundles| {
            bundles
                .into_iter()
                .map(|bundle| {
                    BundleInfo::new(
                        bundle.path().to_path_buf(),
                        bundle.name().to_string(),
                        bundle.namespace().to_string(),
                    )
                })
                .collect()
        })
    }

    /// Return the sorted union of command names across all bundles.
    pub fn commands(&self) -> Result<Vec<String>> {
        let mut commands = self
            .bundles()?
            .into_iter()
            .flat_map(|bundle| bundle.commands())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        commands.sort();
        Ok(commands)
    }
}

impl fmt::Debug for Stack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Stack(name='{}', namespace='{}', path={})",
            self.name,
            self.namespace,
            self.path.display()
        )
    }
}

impl fmt::Display for Stack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, formatter)
    }
}

#[derive(Debug)]
struct StackContext {
    raw: String,
}

impl StackContext {
    fn new(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() || raw.split(':').any(str::is_empty) {
            return Err(EnvoyError::Validation(format!(
                "Invalid empty stack context: {raw:?}"
            )));
        }
        Ok(Self {
            raw: raw.to_string(),
        })
    }

    fn most_specific_levels(&self, max_depth: usize) -> Vec<String> {
        let parts = self.raw.split(':').collect::<Vec<_>>();
        let limit = if max_depth == 0 {
            parts.len()
        } else {
            max_depth.min(parts.len())
        };

        (1..=parts.len())
            .rev()
            .take(limit)
            .map(|length| parts[..length].join(":"))
            .collect()
    }
}

fn load_registry_stacks() -> Result<Vec<Stack>> {
    list_named_stacks()
        .iter()
        .map(Stack::from_named_entry)
        .collect()
}

fn unique_namespace_match(stacks: &[Stack], namespace: &str) -> Result<Option<Stack>> {
    let matches = stacks
        .iter()
        .filter(|stack| stack.namespace == namespace)
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => Ok(None),
        [stack] => Stack::from_name(&stack.name).map(Some),
        _ => Err(EnvoyError::Validation(format!(
            "Multiple stacks match namespace {namespace:?}: {}",
            matches
                .iter()
                .map(|stack| stack.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// What failing to read a stack file that is there means.
///
/// A file that cannot be decoded, or that refuses to be read ("access denied"),
/// is a stack that is wrong: the share answered, so this is not an outage, and
/// a snapshot standing in for it would hide a bad publish or a permissions
/// mistake from everyone who runs it. Anything else, such as a connection that
/// dropped mid-read, says the location cannot be reached.
fn read_failure(path: &Path, source: io::Error) -> Failure {
    let reason = format!("{} cannot be read: {source}", path.display());
    let wrong = matches!(
        source.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::PermissionDenied
    );
    let error = EnvoyError::Io {
        path: path.to_path_buf(),
        source,
    };
    if wrong {
        Failure::Broken(error)
    } else {
        Failure::Unreachable { reason, error }
    }
}

/// A stack entry as a snapshot records it. A published bundle whose marker
/// exists but cannot be read right now keeps what was recorded for it.
fn snapshot_bundle(entry: &StackBundleEntry, recorded: Option<&SnapshotBundle>) -> SnapshotBundle {
    let bundle = &entry.bundle;
    let version = bundle.version();
    let current = SnapshotBundle {
        path: bundle.path().to_path_buf(),
        namespace: bundle.namespace().to_string(),
        name: bundle.name().to_string(),
        version: (version != BUNDLE_CHECKOUT).then_some(version),
        metadata: entry.metadata.clone(),
    };

    match recorded {
        Some(recorded)
            if current.version.is_none()
                && recorded.version.is_some()
                && recorded.path == current.path
                && bundle.is_production() =>
        {
            SnapshotBundle {
                metadata: current.metadata,
                ..recorded.clone()
            }
        }
        _ => current,
    }
}

/// The local stand-in for a bundle in a snapshot: its copy in the store when
/// it is published, or itself when it is a checkout that is on disk.
fn offline_bundle(saved: &SnapshotBundle, store: Option<&Store>) -> Option<Bundle> {
    let info = match &saved.version {
        Some(version) => cached_copy(store?, &saved.namespace, &saved.name, version)?,
        None => saved.path.join(BUNDLE_ENV_DIR).is_dir().then(|| {
            BundleInfo::new(
                saved.path.clone(),
                saved.name.clone(),
                saved.namespace.clone(),
            )
        })?,
    };
    Some(Bundle::from_info(info))
}

fn validate_stack_extension(path: &Path) -> Result<()> {
    if path.extension().and_then(|value| value.to_str()) != Some("estack") {
        return Err(EnvoyError::Validation(format!(
            "Stack files must use the .estack extension: {}",
            path.display()
        )));
    }
    Ok(())
}

fn stack_name_from_path(path: &Path) -> Result<String> {
    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            EnvoyError::Validation(format!(
                "Stack filename must contain a non-empty UTF-8 name: {}",
                path.display()
            ))
        })
}

fn validate_document(path: &Path, document: &StackDocument) -> Result<()> {
    if document.namespace.trim().is_empty() || document.namespace.split(':').any(str::is_empty) {
        return Err(EnvoyError::Validation(format!(
            "Stack namespace must be a non-empty colon-separated context: {}",
            path.display()
        )));
    }
    if document.bundles.is_empty() {
        return Err(EnvoyError::Validation(format!(
            "Stack must contain at least one bundle: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_bundle_documents(path: &Path, bundles: &[StackBundleDocument]) -> Result<()> {
    let mut seen = HashSet::new();
    for bundle in bundles {
        if bundle.path.trim().is_empty() {
            return Err(EnvoyError::Validation(format!(
                "Stack bundle path must not be empty: {}",
                path.display()
            )));
        }
        let resolved = resolve_bundle_path(path, &bundle.path)?;
        if !seen.insert(resolved.clone()) {
            return Err(EnvoyError::Validation(format!(
                "Duplicate bundle path in stack: {}",
                resolved.display()
            )));
        }
        if !validate_bundle(&resolved) {
            return Err(EnvoyError::Validation(format!(
                "Invalid bundle path in stack: {}",
                resolved.display()
            )));
        }
    }
    Ok(())
}

fn resolve_bundle_path(stack_path: &Path, raw_path: &str) -> Result<PathBuf> {
    let expanded = expand_bundle_path(raw_path, stack_path).ok_or_else(|| {
        EnvoyError::Validation(format!(
            "Stack bundle path references an undefined environment variable: {raw_path:?}"
        ))
    })?;
    let path = expand_home_path(&expanded);
    let path = if path.is_absolute() {
        path
    } else {
        stack_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    };
    Ok(resolve_input_path(&path))
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") || path.starts_with(r"~\") {
        if let Some(home) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) {
            let remainder = path
                .strip_prefix("~/")
                .or_else(|| path.strip_prefix(r"~\"))
                .unwrap_or_default();
            return PathBuf::from(home).join(remainder);
        }
    }
    PathBuf::from(path)
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::{
        read_failure, Failure, Stack, DEFAULT_STACK_MAX_DEPTH, DEFAULT_STACK_NAMESPACE,
        STACK_CONTEXT_VAR, STACK_VAR,
    };
    use crate::bundle_store::{load_registry_for_run_with, warm, CacheSettings, Store};
    use crate::discovery::BundleInfo;
    use crate::error::EnvoyError;
    use crate::path_test::assert_same_path;
    use crate::runtime::{apply_bundle_overrides, load_registry};
    use crate::stack_registry::STACK_ROOTS_VAR;
    use envoy_store::{EntryKey, Quiet};

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: Option<&OsStr>) -> Self {
            let previous = env::var_os(key);
            match value {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }

    fn create_bundle(parent: &Path, name: &str) -> PathBuf {
        let bundle = parent.join(name);
        fs::create_dir_all(bundle.join(".envoy")).expect("failed to create bundle fixture");
        fs::write(
            bundle.join(".envoy").join("commands.json"),
            r#"{"python": {}}"#,
        )
        .expect("failed to write commands fixture");
        bundle
    }

    fn stack_yaml(namespace: &str, bundle_path: &str) -> String {
        let bundle_path = bundle_path.replace('\'', "''");
        format!(
            "namespace: {namespace}\nmetadata:\n  owner: tools\nbundles:\n  - path: '{bundle_path}'\n    metadata:\n      role: core\n"
        )
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }

    fn write_named_stack(
        root: &Path,
        name: &str,
        namespace: &str,
        bundle: &Path,
        version: &str,
    ) -> bool {
        let name_dir = root.join(name);
        let version_dir = name_dir.join(version);
        fs::create_dir_all(&version_dir).expect("failed to create stack version directory");
        let filename = format!("{name}.estack");
        fs::write(
            version_dir.join(&filename),
            stack_yaml(namespace, &bundle.to_string_lossy()),
        )
        .expect("failed to write named stack");
        create_file_symlink(
            &Path::new(version).join(filename),
            &name_dir.join("latest.estack"),
        )
    }

    #[test]
    fn stack_loads_strict_yaml_and_resolves_relative_bundles() {
        let temp = tempdir().expect("tempdir should be created");
        let bundle = create_bundle(temp.path(), "bundle");
        let stack_path = temp.path().join("studio.estack");
        fs::write(&stack_path, stack_yaml("team:project", "bundle"))
            .expect("failed to write stack");

        let stack = Stack::new(&stack_path).expect("stack should load");
        assert_eq!(stack.name(), "studio");
        assert_eq!(stack.namespace(), "team:project");
        assert_eq!(stack.registry_version(), None);
        assert_eq!(
            stack
                .metadata()
                .get("owner")
                .and_then(|value| value.as_str()),
            Some("tools")
        );
        assert_same_path(
            stack.bundles().expect("bundles should load")[0].path(),
            &bundle,
        );
        assert_eq!(
            stack.commands().expect("commands should load"),
            vec!["python"]
        );
        assert_eq!(
            stack.bundle_entries().expect("entries should load")[0]
                .metadata
                .get("role")
                .and_then(|value| value.as_str()),
            Some("core")
        );
    }

    #[test]
    fn stack_bundles_take_their_identity_from_the_published_marker() {
        let temp = tempdir().expect("tempdir should be created");
        let python = create_bundle(&temp.path().join("ext").join("python"), "3.11.9-envoy.2");
        fs::write(
            python.join(".bundle"),
            r#"{"bndlid": "ext:python", "name": "python", "version": "3.11.9-envoy.2"}"#,
        )
        .expect("failed to write marker");
        let globals = create_bundle(&temp.path().join("globals"), "0.3.0");
        fs::write(
            globals.join(".bundle"),
            r#"{"bndlid": "globals", "name": "globals", "version": "0.3.0"}"#,
        )
        .expect("failed to write marker");
        let unmarked = create_bundle(&temp.path().join("tools"), "render");

        let stack_path = temp.path().join("studio.estack");
        let entries = [&python, &globals, &unmarked]
            .iter()
            .map(|path| format!("  - path: '{}'\n", path.to_string_lossy()))
            .collect::<String>();
        fs::write(&stack_path, format!("namespace: gt\nbundles:\n{entries}"))
            .expect("failed to write stack");

        let stack = Stack::new(&stack_path).expect("stack should load");
        let ids = stack
            .bundle_infos()
            .expect("bundles should load")
            .iter()
            .map(|info| info.bndlid())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["ext:python", "gt:globals", "tools:render"]);
    }

    #[test]
    fn stack_bundles_match_bundle_overrides_by_their_published_identity() {
        let temp = tempdir().expect("tempdir should be created");
        let published = create_bundle(&temp.path().join("envoy"), "v0.7.0");
        fs::write(
            published.join(".bundle"),
            r#"{"bndlid": "gt:envoy", "name": "envoy", "version": "v0.7.0"}"#,
        )
        .expect("failed to write marker");
        let checkout = create_bundle(temp.path(), "envoy_checkout");
        let stack_path = temp.path().join("studio.estack");
        fs::write(&stack_path, stack_yaml("gt", &published.to_string_lossy()))
            .expect("failed to write stack");

        let infos = Stack::new(&stack_path)
            .expect("stack should load")
            .bundle_infos()
            .expect("bundles should load");
        let (resolved, problems) =
            apply_bundle_overrides(infos, &[("gt:envoy".to_string(), checkout.clone())]);

        assert!(problems.is_empty(), "unexpected problems: {problems:?}");
        assert_eq!(resolved[0].root, checkout);
        assert_eq!(resolved[0].bndlid(), "gt:envoy");
    }

    #[test]
    fn stack_rejects_wrong_extensions_unknown_fields_and_duplicate_bundles() {
        let temp = tempdir().expect("tempdir should be created");
        let bundle = create_bundle(temp.path(), "bundle");
        let yaml = stack_yaml("gt", &bundle.to_string_lossy());

        let wrong_extension = temp.path().join("studio.yaml");
        fs::write(&wrong_extension, &yaml).expect("failed to write wrong-extension fixture");
        assert!(matches!(
            Stack::new(&wrong_extension),
            Err(EnvoyError::Validation(message)) if message.contains(".estack extension")
        ));

        let unknown = temp.path().join("unknown.estack");
        fs::write(&unknown, format!("{yaml}unexpected: true\n"))
            .expect("failed to write unknown-field fixture");
        assert!(matches!(
            Stack::new(&unknown),
            Err(EnvoyError::Validation(_))
        ));

        let obsolete_name = temp.path().join("obsolete.estack");
        fs::write(&obsolete_name, format!("name: obsolete\n{yaml}"))
            .expect("failed to write obsolete-name fixture");
        assert!(matches!(
            Stack::new(&obsolete_name),
            Err(EnvoyError::Validation(_))
        ));

        let duplicate = temp.path().join("duplicate.estack");
        let escaped = bundle.to_string_lossy().replace('\'', "''");
        fs::write(
            &duplicate,
            format!("bundles:\n  - path: '{escaped}'\n  - path: '{escaped}'\n"),
        )
        .expect("failed to write duplicate fixture");
        assert!(matches!(
            Stack::new(&duplicate),
            Err(EnvoyError::Validation(message)) if message.contains("Duplicate bundle path")
        ));
    }

    #[test]
    fn stack_resolution_prefers_the_most_specific_namespace() {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempdir().expect("tempdir should be created");
        let root = temp.path().join("stacks");
        let broad_bundle = create_bundle(temp.path(), "broad-bundle");
        let specific_bundle = create_bundle(temp.path(), "specific-bundle");
        if !write_named_stack(&root, "broad", "team", &broad_bundle, "2026-01-01T00-00-00") {
            return;
        }
        if !write_named_stack(
            &root,
            "specific",
            "team:project",
            &specific_bundle,
            "2026-01-02T00-00-00",
        ) {
            return;
        }
        let roots = env::join_paths([root]).expect("failed to join stack roots");
        let _guard = EnvVarGuard::set(STACK_ROOTS_VAR, Some(roots.as_os_str()));

        let stack = Stack::resolve(
            "team:project:feature",
            DEFAULT_STACK_NAMESPACE,
            DEFAULT_STACK_MAX_DEPTH,
        )
        .expect("context should resolve");
        assert_eq!(stack.name(), "specific");
        assert_eq!(stack.registry_version(), Some("2026-01-02T00-00-00"));
    }

    #[test]
    fn stack_resolution_rejects_ambiguous_namespaces() {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempdir().expect("tempdir should be created");
        let root = temp.path().join("stacks");
        let bundle_a = create_bundle(temp.path(), "bundle-a");
        let bundle_b = create_bundle(temp.path(), "bundle-b");
        if !write_named_stack(&root, "alpha", "team", &bundle_a, "2026-01-01T00-00-00") {
            return;
        }
        if !write_named_stack(&root, "beta", "team", &bundle_b, "2026-01-01T00-00-00") {
            return;
        }
        let roots = env::join_paths([root]).expect("failed to join stack roots");
        let _guard = EnvVarGuard::set(STACK_ROOTS_VAR, Some(roots.as_os_str()));

        assert!(matches!(
            Stack::resolve("team:project", DEFAULT_STACK_NAMESPACE, DEFAULT_STACK_MAX_DEPTH),
            Err(EnvoyError::Validation(message)) if message.contains("Multiple stacks")
        ));
    }

    #[test]
    fn current_stack_honors_environment_user_and_context_precedence() {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempdir().expect("tempdir should be created");
        let root = temp.path().join("stacks");
        let environment_bundle = create_bundle(temp.path(), "environment-bundle");
        let user_bundle = create_bundle(temp.path(), "user-bundle");
        let context_bundle = create_bundle(temp.path(), "context-bundle");
        let environment_stack = temp.path().join("environment.estack");
        let user_stack = temp.path().join("user.estack");
        let user_config = temp.path().join("user_config.json");

        fs::write(
            &environment_stack,
            stack_yaml("gt", &environment_bundle.to_string_lossy()),
        )
        .expect("failed to write environment stack");
        fs::write(
            &user_stack,
            stack_yaml("gt", &user_bundle.to_string_lossy()),
        )
        .expect("failed to write user stack");
        fs::write(
            &user_config,
            serde_json::json!({"stack": user_stack}).to_string(),
        )
        .expect("failed to write user config");
        if !write_named_stack(
            &root,
            "context",
            "team:project",
            &context_bundle,
            "2026-01-03T00-00-00",
        ) {
            return;
        }

        let roots = env::join_paths([root]).expect("failed to join stack roots");
        let _roots_guard = EnvVarGuard::set(STACK_ROOTS_VAR, Some(roots.as_os_str()));
        let _config_root_guard =
            EnvVarGuard::set("ENVOY_CONFIG_ROOT", Some(temp.path().as_os_str()));
        let _context_guard = EnvVarGuard::set(STACK_CONTEXT_VAR, Some(OsStr::new("team:project")));
        let _stack_guard = EnvVarGuard::set(STACK_VAR, None);

        env::set_var(STACK_VAR, &environment_stack);
        let stack = Stack::current(
            false,
            None,
            DEFAULT_STACK_NAMESPACE,
            DEFAULT_STACK_MAX_DEPTH,
        )
        .expect("environment stack should resolve")
        .expect("environment stack should be selected");
        assert_eq!(stack.name(), "environment");

        env::remove_var(STACK_VAR);
        let stack = Stack::current(
            false,
            None,
            DEFAULT_STACK_NAMESPACE,
            DEFAULT_STACK_MAX_DEPTH,
        )
        .expect("user stack should resolve")
        .expect("user stack should be selected");
        assert_eq!(stack.name(), "user");

        let stack = Stack::current(true, None, DEFAULT_STACK_NAMESPACE, DEFAULT_STACK_MAX_DEPTH)
            .expect("context stack should resolve")
            .expect("context stack should be selected");
        assert_eq!(stack.name(), "context");
    }

    /// A published bundle at `parent/<name>/<version>`, as the share holds it.
    fn publish_bundle(parent: &Path, namespace: &str, name: &str, version: &str) -> PathBuf {
        let bundle = create_bundle(&parent.join(name), version);
        fs::write(
            bundle.join(".bundle"),
            format!(
                r#"{{"bndlid": "{namespace}:{name}", "name": "{name}", "version": "{version}"}}"#
            ),
        )
        .expect("failed to write marker");
        bundle
    }

    fn write_stack_of(path: &Path, bundles: &[&Path]) {
        fs::create_dir_all(path.parent().expect("a stack has a folder"))
            .expect("failed to create the stack folder");
        let entries = bundles
            .iter()
            .map(|bundle| {
                format!(
                    "  - path: '{}'\n",
                    bundle.to_string_lossy().replace('\'', "''")
                )
            })
            .collect::<String>();
        fs::write(path, format!("namespace: gt\nbundles:\n{entries}"))
            .expect("failed to write stack");
    }

    /// Copy the first `count` bundles of `stack` into the store under `cache`,
    /// as running them would.
    fn warmed(stack: &Stack, cache: &Path, count: usize) {
        let bundles = stack.bundle_infos().expect("bundles should load");
        let store = Store::open(cache).expect("the store should open");
        warm(&bundles[..count], &store, &Quiet);
    }

    fn ids(stack: &Stack) -> Vec<String> {
        stack
            .bundle_infos()
            .expect("bundles should load")
            .iter()
            .map(BundleInfo::bndlid)
            .collect()
    }

    /// A stack of two published bundles on a share, and a cache beside it.
    struct Studio {
        _temp: tempfile::TempDir,
        share: PathBuf,
        stack: PathBuf,
        cache: PathBuf,
    }

    impl Studio {
        fn new() -> Self {
            let temp = tempdir().expect("tempdir should be created");
            let share = temp.path().join("share");
            let python = publish_bundle(
                &share.join("bndl").join("ext"),
                "ext",
                "python",
                "3.11.9-envoy.2",
            );
            let globals = publish_bundle(&share.join("bndl").join("gt"), "gt", "globals", "0.3.0");
            let stack = share.join("stacks").join("studio.estack");
            write_stack_of(&stack, &[&python, &globals]);
            Self {
                cache: temp.path().join("cache"),
                share,
                stack,
                _temp: temp,
            }
        }

        fn arg(&self) -> String {
            self.stack.to_string_lossy().into_owned()
        }

        /// Run the stack while its share is up and keep the first `count`
        /// bundles locally.
        fn run(&self, count: usize) {
            let stack = Stack::from_selector(&self.arg(), Some(&self.cache))
                .expect("the stack should load");
            stack.bundle_infos().expect("bundles should load");
            stack
                .save_snapshot(&self.cache)
                .expect("the snapshot should save");
            warmed(&stack, &self.cache, count);
        }

        fn lose_the_share(&self) {
            fs::remove_dir_all(&self.share).expect("the share should be removed");
        }
    }

    #[test]
    fn a_stack_that_has_run_starts_from_the_store_when_its_share_is_gone() {
        let studio = Studio::new();
        studio.run(2);
        studio.lose_the_share();

        let stack = Stack::from_selector(&studio.arg(), Some(&studio.cache))
            .expect("the stack should start without its share");
        let info = stack.snapshot_info().expect("it should be rebuilt");
        assert!(info.reason.contains("cannot be reached"), "{}", info.reason);
        assert!(info.unavailable.is_empty());
        assert_eq!(stack.name(), "studio");
        assert_eq!(stack.namespace(), "gt");
        assert_eq!(ids(&stack), vec!["ext:python", "gt:globals"]);
        assert!(stack.bundle_infos().expect("bundles").iter().all(|bundle| {
            bundle.root.starts_with(&studio.cache) && bundle.envoy_env().is_dir()
        }));
        assert_eq!(
            stack.commands().expect("commands should load"),
            vec!["python"]
        );
        assert!(
            !stack
                .save_snapshot(&studio.cache)
                .expect("saving should not fail"),
            "a rebuilt stack has nothing new to remember"
        );
    }

    #[test]
    fn without_a_cache_root_a_missing_share_is_the_error_it_always_was() {
        let studio = Studio::new();
        studio.run(2);
        studio.lose_the_share();

        assert!(matches!(
            Stack::from_selector(&studio.arg(), None),
            Err(EnvoyError::Validation(message))
                if message.contains("Stack path does not exist")
                    && message.contains("cannot be reached")
        ));
    }

    #[test]
    fn a_stack_that_never_ran_here_says_so() {
        let studio = Studio::new();
        studio.lose_the_share();

        let error = Stack::from_selector(&studio.arg(), Some(&studio.cache))
            .expect_err("there is nothing to fall back on");
        let message = error.to_string();
        assert!(
            message.contains("Stack path does not exist")
                && message.contains("cannot be reached")
                && message.contains("no earlier run of it is remembered"),
            "what was asked for, why it failed and that nothing is remembered: {message}"
        );
    }

    /// Where `Studio` publishes its python bundle.
    fn python_of(studio: &Studio) -> PathBuf {
        studio
            .share
            .join("bndl")
            .join("ext")
            .join("python")
            .join("3.11.9-envoy.2")
    }

    #[test]
    fn a_bundle_republished_in_place_is_followed_by_the_snapshot() {
        let studio = Studio::new();
        studio.run(2);

        // The same path with a new version: nothing in the stack file changed.
        fs::write(
            python_of(&studio).join(".bundle"),
            r#"{"bndlid": "ext:python", "name": "python", "version": "3.11.9-envoy.3"}"#,
        )
        .expect("marker is rewritten");
        let stack =
            Stack::from_selector(&studio.arg(), Some(&studio.cache)).expect("stack should load");
        stack.bundle_infos().expect("bundles should load");
        assert!(
            stack
                .save_snapshot(&studio.cache)
                .expect("saving should not fail"),
            "a new version is a change, even at the same path"
        );
        warmed(&stack, &studio.cache, 2);

        // The old copy goes the way an unused one does, and the share goes too.
        let store = Store::open(&studio.cache).expect("the store should open");
        let old = EntryKey::new("ext", "python", "3.11.9-envoy.2").expect("key is valid");
        fs::remove_dir_all(store.entry_path(&old)).expect("the old copy should be removed");
        studio.lose_the_share();

        let offline = Stack::from_selector(&studio.arg(), Some(&studio.cache))
            .expect("the stack should start without its share");
        let info = offline.snapshot_info().expect("it should be rebuilt");
        assert!(info.unavailable.is_empty(), "{:?}", info.unavailable);
        let bundles = offline.bundle_infos().expect("bundles should load");
        assert!(
            bundles[0].root.ends_with("3.11.9-envoy.3"),
            "the newest version it ran: {:?}",
            bundles[0].root
        );
    }

    #[test]
    fn a_marker_that_cannot_be_read_does_not_replace_a_good_record() {
        let studio = Studio::new();
        studio.run(2);

        // A share that is failing: the marker is there, but it cannot be used.
        fs::write(python_of(&studio).join(".bundle"), "{ not json").expect("marker is damaged");
        let stack =
            Stack::from_selector(&studio.arg(), Some(&studio.cache)).expect("stack should load");
        stack.bundle_infos().expect("bundles should load");
        assert!(
            !stack
                .save_snapshot(&studio.cache)
                .expect("saving should not fail"),
            "what was recorded is kept"
        );
        studio.lose_the_share();

        let offline = Stack::from_selector(&studio.arg(), Some(&studio.cache))
            .expect("the stack should start without its share");
        assert_eq!(ids(&offline), vec!["ext:python", "gt:globals"]);
        assert!(offline
            .snapshot_info()
            .expect("it should be rebuilt")
            .unavailable
            .is_empty());
    }

    #[test]
    fn a_stack_of_checkouts_is_not_remembered_and_leaves_the_cache_alone() {
        let temp = tempdir().expect("tempdir should be created");
        let one = create_bundle(&temp.path().join("work"), "one");
        let stack_path = temp.path().join("dev.estack");
        write_stack_of(&stack_path, &[&one]);
        let cache = temp.path().join("cache");

        let stack = Stack::from_selector(&stack_path.to_string_lossy(), Some(&cache))
            .expect("stack should load");
        stack.bundle_infos().expect("bundles should load");

        assert!(!stack.save_snapshot(&cache).expect("saving should not fail"));
        assert!(
            !cache.exists(),
            "a stack with nothing published has nothing to start from, so nothing is written"
        );
    }

    #[test]
    fn a_stack_file_that_refuses_to_be_read_is_wrong_and_a_dropped_connection_is_an_outage() {
        let path = Path::new("studio.estack");

        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::InvalidData] {
            assert!(
                matches!(
                    read_failure(path, io::Error::from(kind)),
                    Failure::Broken(_)
                ),
                "{kind:?} means the share answered and the stack is wrong"
            );
        }
        for kind in [
            io::ErrorKind::TimedOut,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::NotFound,
            io::ErrorKind::Other,
        ] {
            assert!(
                matches!(
                    read_failure(path, io::Error::from(kind)),
                    Failure::Unreachable { .. }
                ),
                "{kind:?} means the location cannot be reached"
            );
        }
    }

    #[test]
    fn bundles_missing_from_the_store_are_listed_and_a_stack_with_none_is_refused() {
        let studio = Studio::new();
        studio.run(0);
        studio.lose_the_share();
        let error = Stack::from_selector(&studio.arg(), Some(&studio.cache))
            .expect_err("nothing is cached");
        let message = error.to_string();
        assert!(
            message.contains("none of its bundles are cached"),
            "{message}"
        );
        assert!(message.contains("ext:python@3.11.9-envoy.2"), "{message}");

        let studio = Studio::new();
        studio.run(1);
        studio.lose_the_share();
        let stack = Stack::from_selector(&studio.arg(), Some(&studio.cache))
            .expect("part of the stack should start");
        assert_eq!(
            stack.snapshot_info().expect("it was rebuilt").unavailable,
            vec!["gt:globals@0.3.0"]
        );
        assert_eq!(ids(&stack), vec!["ext:python"]);
    }

    #[test]
    fn a_checkout_on_disk_stays_available_and_one_that_is_gone_does_not() {
        let temp = tempdir().expect("tempdir should be created");
        let share = temp.path().join("share");
        let python = publish_bundle(
            &share.join("bndl").join("ext"),
            "ext",
            "python",
            "3.11.9-envoy.2",
        );
        let local = create_bundle(&temp.path().join("work"), "local");
        let remote = create_bundle(&share.join("dev"), "remote");
        let stack_path = share.join("stacks").join("studio.estack");
        write_stack_of(&stack_path, &[&python, &local, &remote]);
        let cache = temp.path().join("cache");
        let arg = stack_path.to_string_lossy().into_owned();

        let online = Stack::from_selector(&arg, Some(&cache)).expect("stack should load");
        let online_ids = ids(&online);
        online
            .save_snapshot(&cache)
            .expect("the snapshot should save");
        warmed(&online, &cache, 3);
        fs::remove_dir_all(&share).expect("the share should be removed");

        let offline = Stack::from_selector(&arg, Some(&cache)).expect("it should start");
        assert_eq!(
            ids(&offline),
            vec![online_ids[0].clone(), online_ids[1].clone()]
        );
        assert_eq!(
            offline.snapshot_info().expect("rebuilt").unavailable,
            vec![online_ids[2].clone()]
        );
        let bundles = offline.bundle_infos().expect("bundles should load");
        assert_same_path(&bundles[1].root, &local);
    }

    #[test]
    fn the_snapshot_is_rewritten_only_when_the_stack_changes() {
        let studio = Studio::new();
        let first = Stack::from_selector(&studio.arg(), Some(&studio.cache)).expect("loads");
        first.bundle_infos().expect("bundles should load");
        assert!(first.save_snapshot(&studio.cache).expect("saves"), "new");

        let second = Stack::from_selector(&studio.arg(), Some(&studio.cache)).expect("loads");
        second.bundle_infos().expect("bundles should load");
        assert!(
            !second.save_snapshot(&studio.cache).expect("saves"),
            "unchanged"
        );

        let python = studio
            .share
            .join("bndl")
            .join("ext")
            .join("python")
            .join("3.11.9-envoy.2");
        write_stack_of(&studio.stack, &[&python]);
        let third = Stack::from_selector(&studio.arg(), Some(&studio.cache)).expect("loads");
        third.bundle_infos().expect("bundles should load");
        assert!(
            third.save_snapshot(&studio.cache).expect("saves"),
            "changed"
        );
    }

    #[test]
    fn a_stack_that_can_be_reached_is_never_replaced_by_its_snapshot() {
        let studio = Studio::new();
        studio.run(2);

        fs::write(&studio.stack, "namespace: gt\nbundles: [").expect("stack is broken");
        assert!(matches!(
            Stack::from_selector(&studio.arg(), Some(&studio.cache)),
            Err(EnvoyError::Validation(message)) if message.contains("Invalid YAML")
        ));

        fs::remove_file(&studio.stack).expect("stack is taken away");
        assert!(matches!(
            Stack::from_selector(&studio.arg(), Some(&studio.cache)),
            Err(EnvoyError::Validation(message)) if message.contains("does not exist")
        ));
    }

    #[test]
    fn a_named_stack_starts_from_its_snapshot_only_while_its_root_cannot_be_reached() {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempdir().expect("tempdir should be created");
        let share = temp.path().join("share");
        let python = publish_bundle(
            &share.join("bndl").join("ext"),
            "ext",
            "python",
            "3.11.9-envoy.2",
        );
        let published = share
            .join("stacks")
            .join("studio")
            .join("2026-09-27")
            .join("studio.estack");
        write_stack_of(&published, &[&python]);
        let cache = temp.path().join("cache");
        let roots = env::join_paths([share.join("stacks")]).expect("roots should join");
        let _guard = EnvVarGuard::set(STACK_ROOTS_VAR, Some(roots.as_os_str()));

        let online = Stack::from_selector("studio", Some(&cache)).expect("stack should load");
        assert_eq!(online.registry_version(), Some("2026-09-27"));
        online.bundle_infos().expect("bundles should load");
        assert!(online.save_snapshot(&cache).expect("saves"));
        warmed(&online, &cache, 1);
        fs::remove_dir_all(&share).expect("the share should be removed");

        let offline = Stack::from_selector("studio", Some(&cache)).expect("it should start");
        assert_eq!(offline.registry_version(), Some("2026-09-27"));
        let reason = &offline.snapshot_info().expect("rebuilt").reason;
        assert!(
            reason.contains("stack root") && reason.contains("cannot be reached"),
            "{reason}"
        );
        assert_eq!(ids(&offline), vec!["ext:python"]);

        let error = Stack::from_selector("never-ran", Some(&cache))
            .expect_err("only a stack that ran has a snapshot");
        assert!(error.to_string().contains("no earlier run"), "{error}");

        // The root is back and no longer publishes the name: an error, not a
        // reason to use the copy from before.
        fs::create_dir_all(share.join("stacks")).expect("the root should return");
        assert!(matches!(
            Stack::from_selector("studio", Some(&cache)),
            Err(EnvoyError::Validation(message)) if message.contains("not found in ENVOY_STACK_ROOTS")
        ));
    }

    #[test]
    fn a_run_through_the_python_api_starts_from_the_store_when_the_share_is_gone() {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let studio = Studio::new();
        let python_env = studio
            .share
            .join("bndl")
            .join("ext")
            .join("python")
            .join("3.11.9-envoy.2")
            .join(".envoy");
        fs::write(
            python_env.join("commands.json"),
            r#"{"python": {"environment": ["python_env.json"]}}"#,
        )
        .expect("failed to write commands");
        fs::write(python_env.join("python_env.json"), "{}").expect("failed to write environment");
        let _stack_guard = EnvVarGuard::set(STACK_VAR, Some(studio.stack.as_os_str()));
        let settings = CacheSettings::resolve_with(|_| None, None, studio.cache.clone())
            .with_root(Some(studio.cache.clone()));
        assert!(settings.enabled);

        let (registry, _) = load_registry_for_run_with(None, None, "python", None, &settings)
            .expect("the run should load while the share is up");
        assert!(registry.contains("python"));
        studio.lose_the_share();

        let (registry, bundles) = load_registry_for_run_with(None, None, "python", None, &settings)
            .expect("the run should load without the share");
        assert!(registry.contains("python"));
        assert!(bundles
            .expect("bundles should be discovered")
            .iter()
            .any(|bundle| bundle.root.starts_with(&studio.cache)));

        // Only a caller about to run something may settle for a remembered stack.
        assert!(load_registry(None, None, None).is_err());
    }

    #[test]
    fn the_environment_stack_falls_back_when_one_of_two_roots_is_down() {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempdir().expect("tempdir should be created");
        let local = temp.path().join("local-stacks");
        fs::create_dir_all(&local).expect("local root should be created");
        let share = temp.path().join("share");
        let python = publish_bundle(
            &share.join("bndl").join("ext"),
            "ext",
            "python",
            "3.11.9-envoy.2",
        );
        write_stack_of(
            &share
                .join("stacks")
                .join("studio")
                .join("2026-09-27")
                .join("studio.estack"),
            &[&python],
        );
        let cache = temp.path().join("cache");
        let roots = env::join_paths([local, share.join("stacks")]).expect("roots should join");
        let _roots_guard = EnvVarGuard::set(STACK_ROOTS_VAR, Some(roots.as_os_str()));
        let _stack_guard = EnvVarGuard::set(STACK_VAR, Some(OsStr::new("studio")));

        let current = |cache: Option<&Path>| {
            Stack::current_with_snapshots(
                true,
                None,
                DEFAULT_STACK_NAMESPACE,
                DEFAULT_STACK_MAX_DEPTH,
                cache,
            )
        };
        let online = current(Some(&cache))
            .expect("stack should resolve")
            .expect("stack should be selected");
        online.bundle_infos().expect("bundles should load");
        online.save_snapshot(&cache).expect("saves");
        warmed(&online, &cache, 1);
        fs::remove_dir_all(&share).expect("the share should be removed");

        let offline = current(Some(&cache))
            .expect("stack should resolve")
            .expect("stack should be selected");
        assert!(offline.snapshot_info().is_some());
        assert!(
            current(None).is_err(),
            "without a cache root the stack is simply not found"
        );
        assert!(
            Stack::current(true, None, DEFAULT_STACK_NAMESPACE, DEFAULT_STACK_MAX_DEPTH).is_err()
        );
    }
}
