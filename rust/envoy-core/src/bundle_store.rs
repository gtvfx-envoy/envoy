//! Runs published bundles from the local store.
//!
//! This is where [`envoy_store`] meets the rest of envoy. It reads the caching
//! settings, works out which bundles a command needs, copies those into the
//! store the first time, and swaps their roots for the local copies.
//!
//! Only a command that is about to run copies anything. Listing, inspecting,
//! and diagnosing read bundle metadata from the share and leave it there.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use envoy_store::{EntryKey, Lease, Observer, Outcome, PrunePolicy, Store, StoreError};

use crate::commands::CommandRegistry;
use crate::discovery::{is_published_bundle, Bundle, BundleInfo, BUNDLE_CHECKOUT};
use crate::runtime::collect_env_files;
use crate::user_config::{config_root, UserConfig};

/// `on` or `off`: turn bundle caching on or off. Wins over every other setting.
pub const CACHE_VAR: &str = "ENVOY_CACHE";

/// Directory holding the local store.
pub const CACHE_ROOT_VAR: &str = "ENVOY_CACHE_ROOT";

/// Days an entry may sit unused before prune removes it.
pub const CACHE_MAX_IDLE_DAYS_VAR: &str = "ENVOY_CACHE_MAX_IDLE_DAYS";

/// Size the store may grow to before the least recently used entries go, such
/// as `500GB`. Unset means no limit.
pub const CACHE_MAX_SIZE_VAR: &str = "ENVOY_CACHE_MAX_SIZE";

/// User-config setting that turns caching on or off.
pub const CACHE_SETTING: &str = "cache";

const LEGACY_DISABLE_VAR: &str = crate::bundle_cache::BUNDLE_CACHE_DISABLE_VAR;
const LEGACY_ROOT_VAR: &str = crate::bundle_cache::BUNDLE_CACHE_DIR_VAR;
const LEGACY_ROOT_SETTING: &str = crate::bundle_cache::BUNDLE_CACHE_DIR_SETTING;

/// Days of disuse before an entry is removed, unless configured otherwise.
pub const DEFAULT_MAX_IDLE_DAYS: u64 = 30;

/// Prune runs on an ordinary command at most this often.
const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How often a non-interactive run reports progress.
const LINE_PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

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
        let non_empty = |name: &str| env(name).filter(|value| !value.trim().is_empty());
        let setting = |key: &str| {
            user_config
                .and_then(|config| config.get(key))
                .filter(|value| !value.trim().is_empty())
        };

        let (enabled, enabled_by) =
            if let Some(on) = non_empty(CACHE_VAR).as_deref().and_then(parse_switch) {
                (on, CACHE_VAR)
            } else if non_empty(LEGACY_DISABLE_VAR)
                .as_deref()
                .and_then(parse_switch)
                == Some(true)
            {
                (false, LEGACY_DISABLE_VAR)
            } else if let Some(on) = setting(CACHE_SETTING).and_then(parse_switch) {
                (on, "the cache user setting")
            } else {
                (true, "the default")
            };

        let root = non_empty(CACHE_ROOT_VAR)
            .or_else(|| non_empty(LEGACY_ROOT_VAR))
            .or_else(|| setting(LEGACY_ROOT_SETTING).map(str::to_string))
            .map_or_else(|| config_root.join("cache"), PathBuf::from);

        let max_idle_days = non_empty(CACHE_MAX_IDLE_DAYS_VAR)
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_MAX_IDLE_DAYS);

        Self {
            enabled,
            enabled_by,
            root,
            max_idle: Duration::from_secs(max_idle_days * 24 * 60 * 60),
            max_bytes: non_empty(CACHE_MAX_SIZE_VAR)
                .as_deref()
                .and_then(parse_size),
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

    /// The prune policy these settings describe.
    pub fn prune_policy(&self) -> PrunePolicy {
        PrunePolicy {
            max_bytes: self.max_bytes,
            ..PrunePolicy::idle_for(self.max_idle)
        }
    }
}

/// Parse `on`/`off`, `true`/`false`, `yes`/`no`, or `1`/`0`.
pub fn parse_switch(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
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

/// Shows what the store is doing on standard error.
///
/// On a terminal, progress rewrites one line. Otherwise a line is printed
/// every few seconds, so a log stays readable.
pub struct ProgressPrinter {
    interactive: bool,
    last_line: Mutex<Option<Instant>>,
    drew: AtomicBool,
}

impl ProgressPrinter {
    /// A printer for standard error as it is attached right now.
    pub fn new() -> Self {
        Self {
            interactive: std::io::stderr().is_terminal(),
            last_line: Mutex::new(None),
            drew: AtomicBool::new(false),
        }
    }
}

impl Default for ProgressPrinter {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer for ProgressPrinter {
    fn waiting(&self, key: &EntryKey) {
        eprintln!("envoy: waiting for another envoy to finish caching {key}...");
    }

    fn started(&self, key: &EntryKey, files: u64, bytes: u64) {
        eprintln!(
            "envoy: caching {key} ({files} files, {}) on first use...",
            format_bytes(bytes)
        );
    }

    fn progress(&self, key: &EntryKey, done: u64, total: u64) {
        let percent = (done * 100).checked_div(total).unwrap_or(100);
        if self.interactive {
            eprint!(
                "\r  {key}: {percent}% ({} of {})   ",
                format_bytes(done),
                format_bytes(total)
            );
            let _ = std::io::stderr().flush();
            self.drew.store(true, Ordering::Relaxed);
            return;
        }

        let mut last = self
            .last_line
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if last.is_none_or(|printed| printed.elapsed() >= LINE_PROGRESS_INTERVAL) {
            eprintln!("  {key}: {percent}%");
            *last = Some(Instant::now());
        }
    }

    fn finished(&self, _key: &EntryKey, _elapsed: Duration) {
        if self.drew.swap(false, Ordering::Relaxed) {
            eprintln!();
        }
    }
}

/// Format a byte count for people, such as `637.2 MB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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

fn cache_one(bundle: BundleInfo, store: &Store, observer: &dyn Observer) -> BundleInfo {
    let bndlid = bundle.bndlid();

    let version = Bundle::from_info(bundle.clone()).version();
    if version == BUNDLE_CHECKOUT {
        tracing::debug!(bundle_id = %bndlid, "Not cached: its .bundle marker states no version");
        return bundle;
    }
    let key = match EntryKey::new(&bundle.namespace, &bundle.name, &version) {
        Ok(key) => key,
        Err(error) => {
            tracing::debug!(bundle_id = %bndlid, "Not cached: {error}");
            return bundle;
        }
    };

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
            BundleInfo::new(ensured.path, bundle.name.clone(), bundle.namespace.clone())
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
            bundle
        }
    }
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
}

/// Summarize the store, or `None` when it has not been created yet.
///
/// Never creates the store, so looking is free of side effects.
pub fn summarize(settings: &CacheSettings) -> Option<StoreSummary> {
    if !settings.root.join("store.json").is_file() {
        return None;
    }

    let store = Store::open(&settings.root).ok()?;
    let mut summary = StoreSummary::default();
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

/// Prune the store without holding up the command, at most once a day.
///
/// It runs on a thread that is simply abandoned if the process ends first:
/// entries are removed by renaming them aside, so being cut short loses
/// nothing, and the next prune finishes the cleanup.
fn prune_in_background(store: Store, settings: &CacheSettings) {
    let stamp = settings.root.join("last-prune");
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
    fn the_cache_variable_beats_the_legacy_switch() {
        assert!(!settings(&[("ENVOY_CACHE", "off")]).enabled);
        assert!(!settings(&[("ENVOY_DISABLE_BUNDLE_CACHE", "1")]).enabled);

        let explicit = settings(&[("ENVOY_CACHE", "on"), ("ENVOY_DISABLE_BUNDLE_CACHE", "1")]);
        assert!(explicit.enabled);
        assert_eq!(explicit.enabled_by, "ENVOY_CACHE");
    }

    #[test]
    fn the_root_comes_from_the_most_specific_setting() {
        let root = |pairs: &[(&str, &str)]| settings(pairs).root;

        assert_eq!(
            root(&[
                ("ENVOY_CACHE_ROOT", "/new"),
                ("ENVOY_BUNDLE_CACHE", "/legacy")
            ]),
            PathBuf::from("/new")
        );
        assert_eq!(
            root(&[("ENVOY_BUNDLE_CACHE", "/legacy")]),
            PathBuf::from("/legacy")
        );
        assert_eq!(
            root(&[("ENVOY_CACHE_ROOT", "  ")]).file_name().unwrap(),
            "cache"
        );
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
    fn sizes_and_switches_parse() {
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
        assert_eq!(parse_switch(" On "), Some(true));
        assert_eq!(parse_switch("NO"), Some(false));
        assert_eq!(parse_switch("maybe"), None);
    }

    #[test]
    fn bytes_are_formatted_for_people() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(668_150_000), "637.2 MB");
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
}
