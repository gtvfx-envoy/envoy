//! Where the store lives and whether caching is on.
//!
//! Envoy and the shim that launches it both read these settings, so they put
//! things in the same store and honour the same switches. Resolution is a pure
//! function of the environment and the user's saved settings, both passed in.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// `on` or `off`: turn caching on or off. Wins over every other setting.
pub const CACHE_VAR: &str = "ENVOY_CACHE";

/// Directory holding the store.
pub const CACHE_ROOT_VAR: &str = "ENVOY_CACHE_ROOT";

/// Replaces `~/.envoy`, where envoy keeps its settings and, by default, the store.
pub const CONFIG_ROOT_VAR: &str = "ENVOY_CONFIG_ROOT";

/// User setting that turns caching on or off.
pub const CACHE_SETTING: &str = "cache";

/// Older switch: `1`, `true` or `yes` turns caching off.
pub const LEGACY_DISABLE_VAR: &str = "ENVOY_DISABLE_BUNDLE_CACHE";

/// Older name for [`CACHE_ROOT_VAR`].
pub const LEGACY_ROOT_VAR: &str = "ENVOY_BUNDLE_CACHE";

/// Older name for a user setting that holds the store's directory.
pub const LEGACY_ROOT_SETTING: &str = "bundle_cache_dir";

/// Name of the file of saved user settings, inside the config root.
const USER_CONFIG_FILE: &str = "user_config.json";

/// Whether caching is on and where the store is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location {
    /// Whether anything is cached at all.
    pub enabled: bool,
    /// What decided `enabled`, for diagnostics.
    pub enabled_by: &'static str,
    /// Directory holding the store.
    pub root: PathBuf,
}

/// Parse `on`/`off`, `true`/`false`, `yes`/`no`, or `1`/`0`.
pub fn parse_switch(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// Decide whether caching is on and where the store is.
///
/// Caching is on unless something turns it off. In order: [`CACHE_VAR`], the
/// older [`LEGACY_DISABLE_VAR`], then the `cache` setting. The store is in
/// [`CACHE_ROOT_VAR`], else [`LEGACY_ROOT_VAR`], else the `bundle_cache_dir`
/// setting, else `cache` inside `config_root`. Empty values count as unset.
pub fn resolve(
    env: &dyn Fn(&str) -> Option<String>,
    setting: &dyn Fn(&str) -> Option<String>,
    config_root: &Path,
) -> Location {
    let non_empty = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
    let from_env = |name: &str| non_empty(env(name));
    let from_settings = |name: &str| non_empty(setting(name));

    let (enabled, enabled_by) =
        if let Some(on) = from_env(CACHE_VAR).as_deref().and_then(parse_switch) {
            (on, CACHE_VAR)
        } else if from_env(LEGACY_DISABLE_VAR)
            .as_deref()
            .and_then(parse_switch)
            == Some(true)
        {
            (false, LEGACY_DISABLE_VAR)
        } else if let Some(on) = from_settings(CACHE_SETTING)
            .as_deref()
            .and_then(parse_switch)
        {
            (on, "the cache user setting")
        } else {
            (true, "the default")
        };

    let root = from_env(CACHE_ROOT_VAR)
        .or_else(|| from_env(LEGACY_ROOT_VAR))
        .or_else(|| from_settings(LEGACY_ROOT_SETTING))
        .map_or_else(|| config_root.join("cache"), PathBuf::from);

    Location {
        enabled,
        enabled_by,
        root,
    }
}

/// Resolve from this process's environment and the user's saved settings.
pub fn from_environment() -> Location {
    let env = |name: &str| std::env::var(name).ok();
    let root = config_root(&|name| std::env::var_os(name));
    let settings = read_user_settings(&root.join(USER_CONFIG_FILE));
    resolve(&env, &|key| settings.get(key).cloned(), &root)
}

/// The directory envoy keeps its settings in.
///
/// A non-empty [`CONFIG_ROOT_VAR`] replaces the default `.envoy` in the home
/// directory. If no home directory can be found, a relative `.envoy` is used
/// rather than failing.
pub fn config_root(env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    let non_empty = |name: &str| env(name).filter(|value| !value.is_empty());

    if let Some(root) = non_empty(CONFIG_ROOT_VAR) {
        return PathBuf::from(root);
    }
    home_directory(&non_empty).join(".envoy")
}

#[cfg(windows)]
fn home_directory(env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    if let Some(profile) = env("USERPROFILE") {
        return PathBuf::from(profile);
    }
    if let (Some(mut drive), Some(path)) = (env("HOMEDRIVE"), env("HOMEPATH")) {
        drive.push(path);
        return PathBuf::from(drive);
    }
    env("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

#[cfg(not(windows))]
fn home_directory(env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    env("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Read the user's saved settings: a JSON object, where `//` and `/* */`
/// comments are allowed. Anything unreadable gives no settings, so a damaged
/// file never stops envoy from starting.
///
/// Values that are not strings are kept as their JSON text, matching how the
/// rest of envoy reads the same file.
pub fn read_user_settings(path: &Path) -> HashMap<String, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let stripped = json_comments::StripComments::new(text.as_bytes());
    let Ok(Value::Object(entries)) = serde_json::from_reader::<_, Value>(stripped) else {
        return HashMap::new();
    };

    entries
        .into_iter()
        .map(|(key, value)| match value {
            Value::String(text) => (key, text),
            other => (key, other.to_string()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tempfile::tempdir;

    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn resolved(env: &[(&str, &str)], settings: &[(&str, &str)]) -> Location {
        resolve(
            &lookup(env),
            &lookup(settings),
            Path::new("/home/me/.envoy"),
        )
    }

    #[test]
    fn caching_is_on_by_default_in_the_config_root() {
        let location = resolved(&[], &[]);

        assert!(location.enabled);
        assert_eq!(location.enabled_by, "the default");
        assert_eq!(location.root, Path::new("/home/me/.envoy").join("cache"));
    }

    #[test]
    fn the_switch_is_decided_by_the_most_specific_setting() {
        assert!(!resolved(&[("ENVOY_CACHE", "off")], &[]).enabled);
        assert!(!resolved(&[("ENVOY_DISABLE_BUNDLE_CACHE", "1")], &[]).enabled);
        assert!(!resolved(&[], &[("cache", "no")]).enabled);

        let beaten = resolved(
            &[("ENVOY_CACHE", "on"), ("ENVOY_DISABLE_BUNDLE_CACHE", "1")],
            &[],
        );
        assert!(beaten.enabled && beaten.enabled_by == "ENVOY_CACHE");

        let setting_loses = resolved(&[("ENVOY_CACHE", "on")], &[("cache", "off")]);
        assert!(setting_loses.enabled);

        let unreadable = resolved(&[("ENVOY_CACHE", "maybe")], &[]);
        assert!(
            unreadable.enabled,
            "a value that is not a switch is ignored"
        );
    }

    #[test]
    fn the_root_comes_from_the_most_specific_setting() {
        let all = resolved(
            &[("ENVOY_CACHE_ROOT", "/a"), ("ENVOY_BUNDLE_CACHE", "/b")],
            &[("bundle_cache_dir", "/c")],
        );
        assert_eq!(all.root, Path::new("/a"));

        let legacy = resolved(
            &[("ENVOY_BUNDLE_CACHE", "/b")],
            &[("bundle_cache_dir", "/c")],
        );
        assert_eq!(legacy.root, Path::new("/b"));

        let setting = resolved(&[], &[("bundle_cache_dir", "/c")]);
        assert_eq!(setting.root, Path::new("/c"));

        let blank = resolved(&[("ENVOY_CACHE_ROOT", "  ")], &[]);
        assert_eq!(blank.root, Path::new("/home/me/.envoy").join("cache"));
    }

    #[test]
    fn switches_parse() {
        for on in ["on", "TRUE", " yes ", "1"] {
            assert_eq!(parse_switch(on), Some(true), "{on}");
        }
        for off in ["off", "False", "no", "0"] {
            assert_eq!(parse_switch(off), Some(false), "{off}");
        }
        assert_eq!(parse_switch("maybe"), None);
    }

    #[test]
    fn the_config_root_is_replaced_by_its_variable() {
        let env = |name: &str| (name == "ENVOY_CONFIG_ROOT").then(|| OsString::from("/custom"));

        assert_eq!(config_root(&env), Path::new("/custom"));
    }

    #[test]
    fn without_a_variable_the_config_root_is_beside_the_home_directory() {
        let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let env = |name: &str| (name == home).then(|| OsString::from("/people/me"));

        assert_eq!(config_root(&env), Path::new("/people/me").join(".envoy"));
        assert_eq!(config_root(&|_| None), Path::new(".").join(".envoy"));
        let blank = |name: &str| (name == "ENVOY_CONFIG_ROOT").then(OsString::new);
        assert_eq!(config_root(&blank), Path::new(".").join(".envoy"));
    }

    #[test]
    fn user_settings_allow_comments_and_keep_non_strings_as_text() {
        let dir = tempdir().expect("tempdir is created");
        let path = dir.path().join("user_config.json");
        fs::write(
            &path,
            "// saved by hand\n{\n  \"cache\": \"off\", /* why */\n  \"retries\": 3\n}\n",
        )
        .expect("settings are written");

        let settings = read_user_settings(&path);

        assert_eq!(settings.get("cache").map(String::as_str), Some("off"));
        assert_eq!(settings.get("retries").map(String::as_str), Some("3"));
    }

    #[test]
    fn missing_or_damaged_settings_are_simply_empty() {
        let dir = tempdir().expect("tempdir is created");

        assert!(read_user_settings(&dir.path().join("none.json")).is_empty());
        let damaged = dir.path().join("damaged.json");
        fs::write(&damaged, "{ not json").expect("file is written");
        assert!(read_user_settings(&damaged).is_empty());
        let list = dir.path().join("list.json");
        fs::write(&list, "[1, 2]").expect("file is written");
        assert!(read_user_settings(&list).is_empty());
    }
}
