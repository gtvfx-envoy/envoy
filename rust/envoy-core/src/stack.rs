//! Runtime stack loading and context-aware resolution.
//!
//! A stack is a strict YAML document with an `.estack` extension. Its filename
//! supplies its name, and its document defines an ordered collection of envoy
//! bundles that together form an isolated runtime environment. Named stacks
//! may be published to versioned directories under
//! [`ENVOY_STACK_ROOTS`](crate::stack_registry::STACK_ROOTS_VAR).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::discovery::{
    expand_bundle_path, resolve_input_path, validate_bundle, Bundle, BundleInfo,
};
use crate::error::{EnvoyError, Result};
use crate::stack_registry::{
    is_stack_name, list_named_stacks, resolve_named_stack, NamedStackEntry,
};
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
        let version = path
            .parent()
            .and_then(Path::file_name)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();

        Self::load(&path, Some(name), Some(&version))
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
        if let Some(raw) = non_empty_env(STACK_VAR) {
            return resolve_stack_value(&raw).map(Some);
        }

        if !ignore_user_config {
            let user_config = UserConfig::load(None);
            if let Some(raw) = user_config.get(STACK_SETTING) {
                return resolve_stack_value(raw).map(Some);
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
        let path = resolve_input_path(path);
        validate_stack_extension(&path)?;
        if !path.is_file() {
            return Err(EnvoyError::Validation(format!(
                "Stack path does not exist: {}",
                path.display()
            )));
        }

        let contents = fs::read_to_string(&path).map_err(|source| EnvoyError::Io {
            path: path.clone(),
            source,
        })?;
        let document: StackDocument = serde_yaml::from_str(&contents).map_err(|source| {
            EnvoyError::Validation(format!(
                "Invalid YAML stack at {}: {source}",
                path.display()
            ))
        })?;

        let name = stack_name_from_path(&path)?;
        validate_document(&path, &document)?;
        if let Some(expected_name) = expected_name {
            if name != expected_name {
                return Err(EnvoyError::Validation(format!(
                    "Stack filename {name:?} does not match registry slot {expected_name:?}"
                )));
            }
        }
        validate_bundle_documents(&path, &document.bundles)?;

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

fn resolve_stack_value(raw: &str) -> Result<Stack> {
    if is_stack_name(raw) {
        Stack::from_name(raw)
    } else {
        Stack::new(raw)
    }
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
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::{
        Stack, DEFAULT_STACK_MAX_DEPTH, DEFAULT_STACK_NAMESPACE, STACK_CONTEXT_VAR, STACK_VAR,
    };
    use crate::error::EnvoyError;
    use crate::path_test::assert_same_path;
    use crate::runtime::apply_bundle_overrides;
    use crate::stack_registry::STACK_ROOTS_VAR;

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
}
