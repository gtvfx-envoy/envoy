//! Runs published bundles from the local store.
//!
//! This is where [`envoy_store`] meets the rest of envoy. It reads the caching
//! settings, works out which bundles a command needs, copies those into the
//! store the first time, and swaps their roots for the local copies.
//!
//! Only a command that is about to run copies anything. Listing, inspecting,
//! and diagnosing read bundle metadata from the share and leave it there.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

pub use envoy_store::settings::{parse_switch, CACHE_ROOT_VAR, CACHE_SETTING, CACHE_VAR};
pub use envoy_store::{
    format_bytes, EntryInfo, Health, ProgressPrinter, PrunePolicy, PruneReport, Store, Verdict,
};
use envoy_store::{EntryKey, Lease, Observer, Outcome, StoreError};

use crate::commands::CommandRegistry;
use crate::discovery::{is_published_bundle, Bundle, BundleInfo, BUNDLE_CHECKOUT};
use crate::runtime::collect_env_files;
use crate::user_config::{config_root, UserConfig};

/// Days an entry may sit unused before prune removes it.
pub const CACHE_MAX_IDLE_DAYS_VAR: &str = "ENVOY_CACHE_MAX_IDLE_DAYS";

/// Size the store may grow to before the least recently used entries go, such
/// as `500GB`. Unset means no limit.
pub const CACHE_MAX_SIZE_VAR: &str = "ENVOY_CACHE_MAX_SIZE";

/// Days of disuse before an entry is removed, unless configured otherwise.
pub const DEFAULT_MAX_IDLE_DAYS: u64 = 30;

/// Prune runs on an ordinary command at most this often.
const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// File in the store root whose modified time records the last background prune.
const LAST_PRUNE_FILE: &str = "last-prune";

/// Everything that decides whether and where bundles are cached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheSettings {
    /// Whether bundles are cached at all.
    pub enabled: bool,
    /// What decided `enabled`, for diagnostics.
    pub enabled_by: &'static str,
    /// Directory holding the store.
    pub root: PathBuf,
    /// Entries unused for longer than this are pruned.
    pub max_idle: Duration,
    /// Size above which the least recently used entries are pruned.
    pub max_bytes: Option<u64>,
    /// Whether an ordinary run may prune in the background.
    pub prune: bool,
}

impl CacheSettings {
    /// Resolve the settings from the environment and, unless
    /// `use_user_config` is `false`, the user config.
    ///
    /// Caching is on unless something turns it off. In order: the
    /// [`CACHE_VAR`] variable, the legacy `ENVOY_DISABLE_BUNDLE_CACHE`
    /// variable, then the `cache` user setting. The store root comes from
    /// [`CACHE_ROOT_VAR`], the legacy `ENVOY_BUNDLE_CACHE` variable, the
    /// `bundle_cache_dir` user setting, then `~/.envoy/cache`.
    pub fn resolve(use_user_config: bool) -> Self {
        let user_config = use_user_config.then(|| UserConfig::load(None));
        Self::resolve_with(
            |name| std::env::var(name).ok(),
            user_config.as_ref(),
            config_root(),
        )
    }

    /// [`CacheSettings::resolve`] with the environment and config supplied.
    pub fn resolve_with(
        env: impl Fn(&str) -> Option<String>,
        user_config: Option<&UserConfig>,
        config_root: PathBuf,
    ) -> Self {
        let location = envoy_store::settings::resolve(
            &env,
            &|key| {
                user_config
                    .and_then(|config| config.get(key))
                    .map(str::to_string)
            },
            &config_root,
        );
        let non_empty = |name: &str| env(name).filter(|value| !value.trim().is_empty());

        let max_idle_days = non_empty(CACHE_MAX_IDLE_DAYS_VAR)
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_IDLE_DAYS);

        Self {
            enabled: location.enabled,
            enabled_by: location.enabled_by,
            root: location.root,
            max_idle: Duration::from_secs(max_idle_days * 24 * 60 * 60),
            max_bytes: non_empty(CACHE_MAX_SIZE_VAR)
                .as_deref()
                .and_then(parse_size),
            prune: true,
        }
    }

    /// The same settings with caching switched on or off by an explicit
    /// request, such as a command-line flag.
    #[must_use]
    pub fn with_enabled(mut self, enabled: Option<bool>, requested_by: &'static str) -> Self {
        if let Some(enabled) = enabled {
            self.enabled = enabled;
            self.enabled_by = requested_by;
        }
        self
    }

    /// The same settings with the store in `root`, when one is given.
    #[must_use]
    pub fn with_root(mut self, root: Option<PathBuf>) -> Self {
        if let Some(root) = root {
            self.root = root;
        }
        self
    }

    /// The same settings with background pruning allowed or not.
    #[must_use]
    pub fn with_prune(mut self, prune: bool) -> Self {
        self.prune = prune;
        self
    }

    /// The prune policy these settings describe.
    pub fn prune_policy(&self) -> PrunePolicy {
        PrunePolicy {
            max_bytes: self.max_bytes,
            ..PrunePolicy::idle_for(self.max_idle)
        }
    }
}

/// Parse a size such as `500GB`, `20 MB`, or a plain number of bytes.
pub fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim().to_ascii_lowercase();
    let digits = value.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == ' ');
    let unit = value[digits.len()..].trim();

    let multiplier: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return None,
    };
    digits.trim().parse::<u64>().ok()?.checked_mul(multiplier)
}

/// Open the store the settings describe, or `None` when caching is off or the
/// store cannot be used. Bundles then load from their source paths.
pub fn open_store(settings: &CacheSettings) -> Option<Store> {
    if !settings.enabled {
        return None;
    }

    match Store::open(&settings.root) {
        Ok(store) => Some(store),
        Err(error) => {
            tracing::warn!(
                cache_root = %settings.root.display(),
                "Bundle caching is unavailable, so bundles load from their source paths: {error}"
            );
            None
        }
    }
}

/// Leases held until the process exits, so prune never removes a bundle an
/// envoy is running from. The operating system releases them on exit.
static LEASES: Mutex<Vec<Lease>> = Mutex::new(Vec::new());

fn hold(lease: Lease) {
    LEASES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(lease);
}

/// Return the roots of the bundles that must be on local disk for
/// `command_name` to run.
///
/// A bundle is needed when it supplies an environment file the command loads
/// (every bundle's `global_env.json` counts), or when it defines the command.
/// Bundles that do nothing for this command are not worth copying.
pub fn contributing_roots(
    registry: &CommandRegistry,
    command_name: &str,
    env_override: Option<&str>,
    bundles: &[BundleInfo],
) -> HashSet<PathBuf> {
    let env_source = env_override.unwrap_or(command_name);
    let mut roots = HashSet::new();

    if let Ok(env_files) = collect_env_files(env_source, registry, Some(bundles)) {
        for env_file in env_files {
            // Nested bundle roots are rare, but the deepest match is the owner.
            let owner = bundles
                .iter()
                .filter(|bundle| env_file.starts_with(&bundle.root))
                .max_by_key(|bundle| bundle.root.as_os_str().len());
            if let Some(owner) = owner {
                roots.insert(owner.root.clone());
            }
        }
    }

    for name in [command_name, env_source] {
        let defining_root = registry
            .get(name)
            .and_then(|command| command.envoy_env_dir.as_deref())
            .and_then(Path::parent);
        if let Some(root) = defining_root {
            roots.insert(root.to_path_buf());
        }
    }

    roots
}

/// Replace the root of each published bundle in `needed` with its cached copy,
/// copying it into the store first when it is not there yet.
///
/// Anything that cannot be cached stays where it is: a checkout, a bundle that
/// is not needed, a bundle whose marker has no usable version, or one the
/// store failed to copy. A failure costs one warning, never the run.
pub fn cache_bundles(
    bundles: Vec<BundleInfo>,
    needed: &HashSet<PathBuf>,
    store: &Store,
    observer: &dyn Observer,
) -> Vec<BundleInfo> {
    let published = published_roots(&bundles, needed);
    cache_published(bundles, &published, store, observer)
}

/// The roots among `needed` that belong to published bundles, the only ones
/// the store can hold.
fn published_roots(bundles: &[BundleInfo], needed: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    bundles
        .iter()
        .filter(|bundle| needed.contains(&bundle.root) && is_published_bundle(&bundle.root))
        .map(|bundle| bundle.root.clone())
        .collect()
}

fn cache_published(
    bundles: Vec<BundleInfo>,
    published: &HashSet<PathBuf>,
    store: &Store,
    observer: &dyn Observer,
) -> Vec<BundleInfo> {
    bundles
        .into_iter()
        .map(|bundle| {
            if published.contains(&bundle.root) {
                cache_one(bundle, store, observer)
            } else {
                bundle
            }
        })
        .collect()
}

const NO_MARKER: &str = "it has no .bundle marker (a checkout or an unpublished bundle)";
const NO_VERSION: &str = "its .bundle marker states no version";

fn cache_one(bundle: BundleInfo, store: &Store, observer: &dyn Observer) -> BundleInfo {
    offer(bundle, store, observer).bundle
}

/// What happened when a bundle was offered to the store.
struct Offered {
    /// The bundle to run: its cached copy, or the original when it stayed.
    bundle: BundleInfo,
    /// `namespace:name@version`, or `namespace:name` when there is no version.
    label: String,
    result: Result<Outcome, Stay>,
}

/// Why a bundle stayed where it is.
enum Stay {
    /// It is never cached, for this reason.
    NotCacheable(String),
    /// The store could not take it. The reason was already reported.
    Failed(String),
}

fn offer(bundle: BundleInfo, store: &Store, observer: &dyn Observer) -> Offered {
    let bndlid = bundle.bndlid();

    let version = Bundle::from_info(bundle.clone()).version();
    if version == BUNDLE_CHECKOUT {
        tracing::debug!(bundle_id = %bndlid, "Not cached: {NO_VERSION}");
        return Offered {
            bundle,
            label: bndlid,
            result: Err(Stay::NotCacheable(NO_VERSION.to_string())),
        };
    }
    let key = match EntryKey::new(&bundle.namespace, &bundle.name, &version) {
        Ok(key) => key,
        Err(error) => {
            tracing::debug!(bundle_id = %bndlid, "Not cached: {error}");
            return Offered {
                bundle,
                label: bndlid,
                result: Err(Stay::NotCacheable(error.to_string())),
            };
        }
    };
    let label = key.to_string();

    match store.ensure(&key, &bundle.root, observer) {
        Ok(ensured) => {
            if let Outcome::Filled { elapsed, bytes, .. } = ensured.outcome {
                eprintln!(
                    "envoy: cached {key} ({}) in {:.1} s",
                    format_bytes(bytes),
                    elapsed.as_secs_f64()
                );
            }
            hold(ensured.lease);
            Offered {
                bundle: BundleInfo::new(
                    ensured.path,
                    bundle.name.clone(),
                    bundle.namespace.clone(),
                ),
                label,
                result: Ok(ensured.outcome),
            }
        }
        Err(error) => {
            match error {
                // Reported on an earlier run; the bundle still runs from its source.
                StoreError::PathTooLong {
                    remembered: true, ..
                } => tracing::debug!(
                    bundle_id = %bndlid,
                    "Not cached, loads from {}: {error}",
                    bundle.root.display()
                ),
                StoreError::PathTooLong { .. } => tracing::warn!(
                    bundle_id = %bndlid,
                    "Could not cache {key}, so it loads from {}: {error}. A shorter cache \
                directory, set with ENVOY_CACHE_ROOT, would fit it",
                    bundle.root.display()
                ),
                _ => tracing::warn!(
                    bundle_id = %bndlid,
                    "Could not cache {key}, so it loads from {}: {error}",
                    bundle.root.display()
                ),
            }
            Offered {
                bundle,
                label,
                result: Err(Stay::Failed(error.to_string())),
            }
        }
    }
}

/// What [`warm`] did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WarmReport {
    /// Bundles copied in just now, as `namespace:name@version`.
    pub filled: Vec<String>,
    /// Bytes copied in just now.
    pub filled_bytes: u64,
    /// Bundles that were already in the store.
    pub already_cached: usize,
    /// Bundles the store could not take, with why. They still run from where
    /// they are.
    pub failed: Vec<(String, String)>,
    /// Bundles that are never cached, such as checkouts, with why.
    pub not_cacheable: Vec<(String, String)>,
}

/// Copy every published bundle in `bundles` into the store ahead of time.
///
/// This is what `envoy --noop` and `envoy cache warm` do, so a first real run
/// finds everything already local.
pub fn warm(bundles: &[BundleInfo], store: &Store, observer: &dyn Observer) -> WarmReport {
    let mut report = WarmReport::default();

    for bundle in bundles {
        if !is_published_bundle(&bundle.root) {
            report
                .not_cacheable
                .push((bundle.bndlid(), NO_MARKER.to_string()));
            continue;
        }
        let offered = offer(bundle.clone(), store, observer);
        match offered.result {
            Ok(Outcome::Hit) => report.already_cached += 1,
            Ok(Outcome::Filled { bytes, .. }) => {
                report.filled.push(offered.label);
                report.filled_bytes += bytes;
            }
            Err(Stay::NotCacheable(reason)) => report.not_cacheable.push((offered.label, reason)),
            Err(Stay::Failed(reason)) => report.failed.push((offered.label, reason)),
        }
    }

    report
}

/// How a bundle stands with the store, for `--diagnose` and `envoy cache`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Standing {
    /// A usable copy is in the store.
    Cached,
    /// It can be cached and has not been yet.
    NotCachedYet,
    /// The store refused it earlier and would again, for this reason.
    Refused(String),
    /// It is never cached, for this reason.
    NotCacheable(String),
}

/// Work out how `bundle` stands with `store`, without copying or creating
/// anything. Pass `None` when the store does not exist yet.
pub fn standing(bundle: &BundleInfo, store: Option<&Store>) -> Standing {
    if !is_published_bundle(&bundle.root) {
        return Standing::NotCacheable(NO_MARKER.to_string());
    }
    let version = Bundle::from_info(bundle.clone()).version();
    if version == BUNDLE_CHECKOUT {
        return Standing::NotCacheable(NO_VERSION.to_string());
    }
    let key = match EntryKey::new(&bundle.namespace, &bundle.name, &version) {
        Ok(key) => key,
        Err(error) => return Standing::NotCacheable(error.to_string()),
    };

    let Some(store) = store else {
        return Standing::NotCachedYet;
    };
    if store.lookup(&key).is_some() {
        Standing::Cached
    } else if let Some(error) = store.refusal(&key) {
        Standing::Refused(error.to_string())
    } else {
        Standing::NotCachedYet
    }
}

/// Load the registry for a caller that is about to run `command`, with the
/// bundles it needs coming from the local store.
///
/// This is `envoy COMMAND` for in-process callers such as the Python API.
/// Callers that only look at a command should use
/// [`load_registry`](crate::runtime::load_registry) directly, so that looking
/// never copies anything.
pub fn load_registry_for_run(
    bundle_roots: Option<&[String]>,
    commands_file: Option<&Path>,
    command: &str,
    env_override: Option<&str>,
) -> crate::error::Result<(CommandRegistry, Option<Vec<BundleInfo>>)> {
    load_registry_for_run_with(
        bundle_roots,
        commands_file,
        command,
        env_override,
        &CacheSettings::resolve(true),
    )
}

/// [`load_registry_for_run`] with the caching settings supplied.
pub fn load_registry_for_run_with(
    bundle_roots: Option<&[String]>,
    commands_file: Option<&Path>,
    command: &str,
    env_override: Option<&str>,
    settings: &CacheSettings,
) -> crate::error::Result<(CommandRegistry, Option<Vec<BundleInfo>>)> {
    let (registry, bundles) = crate::runtime::load_registry(bundle_roots, commands_file, None)?;
    Ok(cache_for_command(
        registry,
        bundles,
        command,
        env_override,
        settings,
    ))
}

/// Run the bundles `command_name` needs from the local store.
///
/// Returns the registry and bundles to run with. They are returned unchanged
/// when caching is off, the store cannot be opened, or nothing needed caching.
/// A run that needs only checkouts and unpublished bundles never touches the
/// store, so it is not even created for someone who has no use for it.
pub fn cache_for_command(
    registry: CommandRegistry,
    bundles: Option<Vec<BundleInfo>>,
    command_name: &str,
    env_override: Option<&str>,
    settings: &CacheSettings,
) -> (CommandRegistry, Option<Vec<BundleInfo>>) {
    let Some(bundle_list) = bundles else {
        return (registry, None);
    };
    if !settings.enabled {
        return (registry, Some(bundle_list));
    }

    let needed = contributing_roots(&registry, command_name, env_override, &bundle_list);
    let published = published_roots(&bundle_list, &needed);
    if published.is_empty() {
        return (registry, Some(bundle_list));
    }
    let Some(store) = open_store(settings) else {
        return (registry, Some(bundle_list));
    };

    let printer = ProgressPrinter::new();
    let cached = cache_published(bundle_list.clone(), &published, &store, &printer);
    if cached == bundle_list {
        prune_in_background(store, settings);
        return (registry, Some(bundle_list));
    }

    // Aliases and environment files resolve against bundle roots, so the
    // registry is rebuilt from the cached roots rather than patched.
    let mut rebuilt = CommandRegistry::empty();
    rebuilt.load_from_bundles(&cached);
    prune_in_background(store, settings);

    if rebuilt.get(command_name).is_some() || env_override.is_some() {
        (rebuilt, Some(cached))
    } else {
        (registry, Some(bundle_list))
    }
}

/// What the store currently holds, for `--diagnose` and `envoy cache status`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreSummary {
    /// Entries a run can use.
    pub entries: usize,
    /// Directories that are not usable entries (incomplete, or from a newer envoy).
    pub unusable: usize,
    /// Total size recorded for the usable entries, in bytes.
    pub bytes: u64,
    /// Bundles the store has found it cannot hold, such as ones whose paths
    /// are too long for the root.
    pub refused: usize,
}

/// Open the store the settings describe if it exists, without creating it.
pub fn existing_store(settings: &CacheSettings) -> Option<Store> {
    Store::open_existing(&settings.root).ok().flatten()
}

/// Filesystem types that live on another machine.
#[cfg(any(target_os = "linux", test))]
const NETWORK_FILESYSTEMS: [&str; 11] = [
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "afs",
    "ceph",
    "glusterfs",
    "lustre",
    "9p",
    "davfs",
];

/// Whether `path` lies on a network location.
///
/// A cache there saves little, since every read still crosses the network, and
/// file locks, which keep envoys from filling the same entry at once, are not
/// reliable on some network filesystems.
#[cfg(windows)]
pub fn on_network(path: &Path) -> bool {
    let text = path.to_string_lossy();
    // `\\?\C:\...` is a local path in its long form.
    (text.starts_with(r"\\") && !text.starts_with(r"\\?\")) || text.starts_with(r"\\?\UNC\")
}

/// Whether `path` lies on a network location (see the Windows version).
#[cfg(target_os = "linux")]
pub fn on_network(path: &Path) -> bool {
    std::fs::read_to_string("/proc/self/mounts")
        .is_ok_and(|mounts| is_on_network_mount(path, &mounts))
}

/// Whether `path` lies on a network location (see the Windows version).
///
/// Not detected on this platform.
#[cfg(not(any(windows, target_os = "linux")))]
pub fn on_network(_path: &Path) -> bool {
    false
}

/// Whether the mount holding `path`, as listed in `mounts` (the text of
/// `/proc/self/mounts`), is a network filesystem.
///
/// The longest mount point that is a prefix of `path` is the one that holds it.
#[cfg(any(target_os = "linux", test))]
fn is_on_network_mount(path: &Path, mounts: &str) -> bool {
    let mut holder: Option<(usize, &str)> = None;

    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(_device), Some(mount_point), Some(file_system)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };

        // The kernel writes a space in a mount point as \040.
        let mount_point = mount_point.replace("\\040", " ");
        if path.starts_with(&mount_point) {
            let length = mount_point.len();
            if holder.is_none_or(|(longest, _)| length >= longest) {
                holder = Some((length, file_system));
            }
        }
    }

    holder.is_some_and(|(_, file_system)| NETWORK_FILESYSTEMS.contains(&file_system))
}

/// When the store was last pruned in the background, if it ever was.
pub fn last_prune(settings: &CacheSettings) -> Option<SystemTime> {
    std::fs::metadata(settings.root.join(LAST_PRUNE_FILE))
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Summarize the store, or `None` when it has not been created yet.
///
/// Never creates the store, so looking is free of side effects.
pub fn summarize(settings: &CacheSettings) -> Option<StoreSummary> {
    let store = existing_store(settings)?;
    let mut summary = StoreSummary {
        refused: store.refusals().len(),
        ..StoreSummary::default()
    };
    for entry in store.list() {
        if entry.usable() {
            summary.entries += 1;
            summary.bytes += entry.bytes();
        } else {
            summary.unusable += 1;
        }
    }
    Some(summary)
}

/// Copy the bundles `command_name` needs into the store ahead of time, as
/// `envoy --noop COMMAND` does.
pub fn warm_command(
    registry: &CommandRegistry,
    bundles: &[BundleInfo],
    command_name: &str,
    env_override: Option<&str>,
    store: &Store,
    observer: &dyn Observer,
) -> WarmReport {
    let needed = contributing_roots(registry, command_name, env_override, bundles);
    let wanted: Vec<BundleInfo> = bundles
        .iter()
        .filter(|bundle| needed.contains(&bundle.root))
        .cloned()
        .collect();
    warm(&wanted, store, observer)
}

/// Prune the store without holding up the command, at most once a day.
///
/// It runs on a thread that is simply abandoned if the process ends first:
/// entries are removed by renaming them aside, so being cut short loses
/// nothing, and the next prune finishes the cleanup.
fn prune_in_background(store: Store, settings: &CacheSettings) {
    if !settings.prune {
        return;
    }

    let stamp = settings.root.join(LAST_PRUNE_FILE);
    let recent = std::fs::metadata(&stamp)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < PRUNE_INTERVAL);
    if recent || std::fs::write(&stamp, b"").is_err() {
        return;
    }

    let policy = settings.prune_policy();
    std::thread::spawn(move || {
        let report = store.prune(&policy);
        if !report.removed.is_empty() {
            tracing::debug!(
                removed = report.removed.len(),
                freed_bytes = report.freed_bytes,
                "Pruned the bundle cache"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn settings(pairs: &[(&str, &str)]) -> CacheSettings {
        CacheSettings::resolve_with(env_of(pairs), None, PathBuf::from("/home/user/.envoy"))
    }

    #[test]
    fn caching_is_on_by_default_under_the_config_root() {
        let defaults = settings(&[]);

        assert!(defaults.enabled);
        assert_eq!(defaults.enabled_by, "the default");
        assert_eq!(
            defaults.root,
            PathBuf::from("/home/user/.envoy").join("cache")
        );
        assert_eq!(defaults.max_idle, Duration::from_secs(30 * 24 * 60 * 60));
        assert_eq!(defaults.max_bytes, None);
    }

    #[test]
    fn retention_settings_are_read_and_bad_values_fall_back() {
        let tuned = settings(&[
            ("ENVOY_CACHE_MAX_IDLE_DAYS", "7"),
            ("ENVOY_CACHE_MAX_SIZE", "500GB"),
        ]);
        assert_eq!(tuned.max_idle, Duration::from_secs(7 * 24 * 60 * 60));
        assert_eq!(tuned.max_bytes, Some(500 * (1 << 30)));

        let bad = settings(&[
            ("ENVOY_CACHE_MAX_IDLE_DAYS", "soon"),
            ("ENVOY_CACHE_MAX_SIZE", "lots"),
        ]);
        assert_eq!(bad.max_idle, Duration::from_secs(30 * 24 * 60 * 60));
        assert_eq!(bad.max_bytes, None);
    }

    #[test]
    fn sizes_parse() {
        for (text, bytes) in [
            ("1024", 1024),
            ("2KB", 2048),
            ("20 mb", 20 << 20),
            ("1T", 1 << 40),
        ] {
            assert_eq!(parse_size(text), Some(bytes), "{text}");
        }
        for text in ["", "GB", "12 parsecs", "-5"] {
            assert_eq!(parse_size(text), None, "{text}");
        }
    }

    /// The shim cannot depend on this crate, so it finds the config root with
    /// the store's copy of this rule. The two must never drift apart.
    #[test]
    fn the_app_and_the_shim_agree_on_the_config_root() {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        assert_eq!(
            config_root(),
            envoy_store::settings::config_root(&|name| std::env::var_os(name))
        );
    }

    #[test]
    fn the_switches_and_the_root_reach_the_settings_unchanged() {
        let off = settings(&[("ENVOY_CACHE", "off"), ("ENVOY_CACHE_ROOT", "/elsewhere")]);

        assert!(!off.enabled);
        assert_eq!(off.enabled_by, "ENVOY_CACHE");
        assert_eq!(off.root, PathBuf::from("/elsewhere"));
    }

    fn published(root: &Path, namespace: &str, name: &str, version: &str) -> BundleInfo {
        let dir = root.join(name).join(version);
        fs::create_dir_all(dir.join(".envoy")).expect(".envoy is created");
        fs::write(
            dir.join(".bundle"),
            json!({"bndlid": format!("{namespace}:{name}"), "name": name, "version": version})
                .to_string(),
        )
        .expect("marker is written");
        BundleInfo::new(dir, name.to_string(), namespace.to_string())
    }

    fn write_env(bundle: &BundleInfo, file_name: &str, contents: &str) -> BundleInfo {
        fs::write(bundle.root.join(".envoy").join(file_name), contents).expect("env file");
        BundleInfo::new(
            bundle.root.clone(),
            bundle.name.clone(),
            bundle.namespace.clone(),
        )
    }

    /// `tool` defines the `run` command, `globals` has a global_env.json, `lib`
    /// supplies the tool_env.json that `run` loads, and `other` does nothing for it.
    fn studio(root: &Path) -> Vec<BundleInfo> {
        let tool = published(root, "gt", "tool", "1.0.0");
        let tool = write_env(
            &tool,
            "commands.json",
            r#"{"run": {"environment": ["tool_env.json"], "alias": ["tool"]}}"#,
        );
        let globals = write_env(
            &published(root, "gt", "globals", "1.0.0"),
            "global_env.json",
            "{}",
        );
        let lib = write_env(
            &published(root, "ext", "lib", "2.0.0"),
            "tool_env.json",
            "{}",
        );
        let other = write_env(
            &published(root, "ext", "other", "3.0.0"),
            "other_env.json",
            "{}",
        );
        vec![tool, globals, lib, other]
    }

    #[test]
    fn only_bundles_that_contribute_to_the_command_are_needed() {
        let temp = tempdir().expect("tempdir is created");
        let bundles = studio(temp.path());
        let mut registry = CommandRegistry::empty();
        registry.load_from_bundles(&bundles);

        let needed = contributing_roots(&registry, "run", None, &bundles);

        let expected: HashSet<PathBuf> = bundles[..3].iter().map(|b| b.root.clone()).collect();
        assert_eq!(needed, expected, "tool, globals and lib, but not other");
    }

    #[test]
    fn needed_published_bundles_run_from_the_store_and_the_rest_stay_put() {
        let temp = tempdir().expect("tempdir is created");
        let bundles = studio(temp.path());
        let mut registry = CommandRegistry::empty();
        registry.load_from_bundles(&bundles);
        let settings = CacheSettings {
            root: temp.path().join("store"),
            ..settings(&[])
        };

        let (registry, cached) =
            cache_for_command(registry, Some(bundles.clone()), "run", None, &settings);

        let cached = cached.expect("bundles are returned");
        for (original, now) in bundles[..3].iter().zip(&cached[..3]) {
            assert!(
                now.root.starts_with(&settings.root),
                "{} is cached",
                now.bndlid()
            );
            assert_eq!(now.bndlid(), original.bndlid());
        }
        assert_eq!(
            cached[3].root, bundles[3].root,
            "an unneeded bundle is left alone"
        );
        let defined_in = registry.get("run").and_then(|c| c.envoy_env_dir.clone());
        assert!(defined_in.is_some_and(|dir| dir.starts_with(&settings.root)));
    }

    #[test]
    fn nothing_is_copied_when_caching_is_off() {
        let temp = tempdir().expect("tempdir is created");
        let bundles = studio(temp.path());
        let mut registry = CommandRegistry::empty();
        registry.load_from_bundles(&bundles);
        let off = CacheSettings {
            root: temp.path().join("store"),
            ..settings(&[("ENVOY_CACHE", "off")])
        };

        let (_, cached) = cache_for_command(registry, Some(bundles.clone()), "run", None, &off);

        assert_eq!(cached, Some(bundles));
        assert!(
            !off.root.exists(),
            "an off switch must not even create the store"
        );
    }

    #[test]
    fn a_run_that_needs_only_checkouts_never_creates_the_store() {
        let temp = tempdir().expect("tempdir is created");
        let checkout = temp.path().join("gt").join("dev");
        fs::create_dir_all(checkout.join(".envoy")).expect("checkout is created");
        fs::write(
            checkout.join(".envoy").join("commands.json"),
            r#"{"dev": {"environment": []}}"#,
        )
        .expect("commands are written");
        let bundles = vec![BundleInfo::new(
            checkout,
            "dev".to_string(),
            "gt".to_string(),
        )];
        let mut registry = CommandRegistry::empty();
        registry.load_from_bundles(&bundles);
        let on = CacheSettings {
            root: temp.path().join("store"),
            ..settings(&[])
        };

        let (_, unchanged) = cache_for_command(registry, Some(bundles.clone()), "dev", None, &on);

        assert_eq!(unchanged, Some(bundles));
        assert!(
            !on.root.exists(),
            "there is nothing to cache, so the store is not created"
        );
    }

    #[test]
    fn a_bundle_without_a_usable_version_stays_on_the_share() {
        let temp = tempdir().expect("tempdir is created");
        let store = Store::open(temp.path().join("store")).expect("store opens");
        let unsafe_version = published(temp.path(), "gt", "odd", "9.9.9");
        fs::write(
            unsafe_version.root.join(".bundle"),
            json!({"bndlid": "gt:odd", "version": "../escape"}).to_string(),
        )
        .expect("marker is rewritten");
        let needed = HashSet::from([unsafe_version.root.clone()]);

        let cached = cache_bundles(
            vec![unsafe_version.clone()],
            &needed,
            &store,
            &envoy_store::Quiet,
        );

        assert_eq!(cached, vec![unsafe_version]);
        assert!(store.list().is_empty());
    }

    #[test]
    fn checkouts_are_never_cached() {
        let temp = tempdir().expect("tempdir is created");
        let store = Store::open(temp.path().join("store")).expect("store opens");
        let checkout = temp.path().join("gt").join("dev");
        fs::create_dir_all(checkout.join(".envoy")).expect("checkout is created");
        let info = BundleInfo::new(checkout.clone(), "dev".to_string(), "gt".to_string());
        let needed = HashSet::from([checkout]);

        let cached = cache_bundles(vec![info.clone()], &needed, &store, &envoy_store::Quiet);

        assert_eq!(cached, vec![info]);
        assert!(store.list().is_empty());
    }

    #[test]
    fn a_mount_is_judged_by_the_longest_mount_point_that_holds_the_path() {
        let mounts = "\
rootfs / ext4 rw 0 0
server:/export /home nfs4 rw 0 0
/dev/sdb1 /home/me/local ext4 rw 0 0
//nas/share /mnt/with\\040space cifs rw 0 0
";

        assert!(is_on_network_mount(
            Path::new("/home/someone/.envoy/cache"),
            mounts
        ));
        assert!(
            !is_on_network_mount(Path::new("/home/me/local/.envoy/cache"), mounts),
            "a local disk mounted inside a network home is local"
        );
        assert!(!is_on_network_mount(Path::new("/var/cache/envoy"), mounts));
        assert!(is_on_network_mount(
            Path::new("/mnt/with space/cache"),
            mounts
        ));
        assert!(!is_on_network_mount(Path::new("relative/cache"), mounts));
    }

    #[cfg(windows)]
    #[test]
    fn a_unc_path_is_on_the_network_and_a_long_local_path_is_not() {
        assert!(on_network(Path::new(r"\\server\share\cache")));
        assert!(on_network(Path::new(r"\\?\UNC\server\share\cache")));
        assert!(!on_network(Path::new(r"\\?\C:\Users\me\.envoy\cache")));
        assert!(!on_network(Path::new(r"C:\Users\me\.envoy\cache")));
    }

    #[test]
    fn warm_copies_every_published_bundle_and_explains_the_rest() {
        let temp = tempdir().expect("tempdir is created");
        let mut bundles = studio(temp.path());
        let checkout = temp.path().join("gt").join("dev");
        fs::create_dir_all(checkout.join(".envoy")).expect("checkout is created");
        bundles.push(BundleInfo::new(
            checkout,
            "dev".to_string(),
            "gt".to_string(),
        ));
        let store = Store::open(temp.path().join("store")).expect("store opens");

        let first = warm(&bundles, &store, &envoy_store::Quiet);

        assert_eq!(first.filled.len(), 4);
        assert!(first.filled.contains(&"gt:tool@1.0.0".to_string()));
        assert_eq!(first.already_cached, 0);
        assert!(first.failed.is_empty());
        assert_eq!(first.not_cacheable.len(), 1);
        assert_eq!(first.not_cacheable[0].0, "gt:dev");
        assert!(first.not_cacheable[0].1.contains(".bundle marker"));

        let second = warm(&bundles, &store, &envoy_store::Quiet);
        assert!(second.filled.is_empty(), "everything is already local");
        assert_eq!(second.already_cached, 4);
    }

    #[test]
    fn warming_a_command_copies_only_what_it_needs() {
        let temp = tempdir().expect("tempdir is created");
        let bundles = studio(temp.path());
        let mut registry = CommandRegistry::empty();
        registry.load_from_bundles(&bundles);
        let store = Store::open(temp.path().join("store")).expect("store opens");

        let report = warm_command(
            &registry,
            &bundles,
            "run",
            None,
            &store,
            &envoy_store::Quiet,
        );

        assert_eq!(report.filled.len(), 3, "tool, globals and lib, not other");
        assert!(!report.filled.contains(&"ext:other@3.0.0".to_string()));
    }

    #[test]
    fn standing_tells_each_kind_of_bundle_apart_without_creating_a_store() {
        let temp = tempdir().expect("tempdir is created");
        let bundles = studio(temp.path());
        let root = temp.path().join("store");

        assert_eq!(standing(&bundles[0], None), Standing::NotCachedYet);
        let store = Store::open(&root).expect("store opens");
        assert_eq!(standing(&bundles[0], Some(&store)), Standing::NotCachedYet);
        warm(&bundles[..1], &store, &envoy_store::Quiet);
        assert_eq!(standing(&bundles[0], Some(&store)), Standing::Cached);

        let checkout = temp.path().join("gt").join("dev");
        fs::create_dir_all(checkout.join(".envoy")).expect("checkout is created");
        let checkout = BundleInfo::new(checkout, "dev".to_string(), "gt".to_string());
        assert!(matches!(
            standing(&checkout, Some(&store)),
            Standing::NotCacheable(reason) if reason.contains("no .bundle marker")
        ));
    }

    #[test]
    fn a_flag_can_override_where_and_whether_to_cache_and_prune() {
        let base = settings(&[]);

        let changed = base
            .clone()
            .with_enabled(Some(false), "--cache")
            .with_root(Some(PathBuf::from("/elsewhere")))
            .with_prune(false);

        assert!(!changed.enabled);
        assert_eq!(changed.enabled_by, "--cache");
        assert_eq!(changed.root, PathBuf::from("/elsewhere"));
        assert!(!changed.prune);

        let untouched = base.clone().with_enabled(None, "--cache").with_root(None);
        assert_eq!(untouched, base, "no flag means no change");
        assert!(base.prune, "pruning is on unless told otherwise");
    }
}
