//! Persistent user configuration for envoy.
//!
//! This module ports `py/envoy/_user_config.py` into `envoy-core`.
//! It stores per-user preferences in a JSON file beneath Envoy's shared
//! cross-platform config root so flags and paths do not need to be repeated
//! on every invocation.
//!
//! The default config file is `~/.envoy/user_config.json` on Windows, macOS,
//! and Linux. Set `ENVOY_CONFIG_ROOT` to replace the `~/.envoy` directory.
//!
//! Settings are stored as a flat JSON object. Use [`UserConfig::load`] to read
//! the config and [`UserConfig::save`] to persist changes.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fmt;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

use crate::error::{EnvoyError, Result};
use crate::json_util::parse_json_with_comments;

/// Environment variable that overrides Envoy's shared config root.
pub const CONFIG_ROOT_VAR: &str = "ENVOY_CONFIG_ROOT";

/// Metadata describing one supported user-config setting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KnownSetting {
    /// Human-readable explanation of what the setting controls.
    pub description: &'static str,
    /// Allowed string values, or `None` when the setting is free-form.
    pub choices: Option<&'static [&'static str]>,
}

const VERBOSITY_CHOICES: &[&str] = &["quiet", "normal", "verbose"];
const CACHE_CHOICES: &[&str] = &["on", "off"];

const KNOWN_SETTINGS: [(&str, KnownSetting); 5] = [
    (
        "stack",
        KnownSetting {
            description: "Path to the default named stack or .estack file.  Used when \
--stack is not supplied on the command line.",
            choices: None,
        },
    ),
    (
        "config_key_file",
        KnownSetting {
            description: "Path to an age identity file used to decrypt opt-in \
encrypted values in `.envoy/*.json` config files. Set to an empty string to leave config \
decryption disabled unless ENVOY_CONFIG_KEY_FILE is set.",
            choices: None,
        },
    ),
    (
        "verbosity",
        KnownSetting {
            description: "Default verbosity level for all envoy invocations.",
            choices: Some(VERBOSITY_CHOICES),
        },
    ),
    (
        "bundle_cache_dir",
        KnownSetting {
            description: "Directory holding the local bundle cache. Set to an empty \
string to fall back to the default location (~/.envoy/cache). The ENVOY_CACHE_ROOT \
and ENVOY_BUNDLE_CACHE environment variables take precedence.",
            choices: None,
        },
    ),
    (
        "cache",
        KnownSetting {
            description: "Whether published bundles are copied to local disk on first \
use and run from there. The ENVOY_CACHE environment variable takes precedence.",
            choices: Some(CACHE_CHOICES),
        },
    ),
];

/// Return the registry of all settings that can be stored in the user config.
///
/// A plain static slice is used instead of a map because the registry is tiny
/// and linear lookup keeps the implementation dependency-free.
pub fn known_settings() -> &'static [(&'static str, KnownSetting)] {
    &KNOWN_SETTINGS
}

/// Return Envoy's default cross-platform config root.
///
/// The root is `~/.envoy` on Windows, macOS, and Linux. On Windows, the home
/// directory is resolved from `%USERPROFILE%`, then `%HOMEDRIVE%%HOMEPATH%`,
/// then `%HOME%`. On other platforms, `$HOME` is used.
///
/// If no home-related environment variable is available, a relative `.envoy`
/// path is returned instead of panicking.
pub fn default_config_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        windows_home_directory().join(".envoy")
    }

    #[cfg(not(target_os = "windows"))]
    {
        non_windows_home_directory().join(".envoy")
    }
}

/// Return Envoy's effective shared config root.
///
/// A non-empty `ENVOY_CONFIG_ROOT` value replaces the default `~/.envoy`
/// directory. The value is read each time this function is called.
pub fn config_root() -> PathBuf {
    non_empty_env_path(CONFIG_ROOT_VAR).unwrap_or_else(default_config_root)
}

/// Return the default user config file path.
pub fn default_config_path() -> PathBuf {
    default_config_root().join("user_config.json")
}

/// Return the effective user config file path.
pub fn user_config_path() -> PathBuf {
    config_root().join("user_config.json")
}

/// Persistent user configuration for envoy.
///
/// The config is loaded from and saved to [`user_config_path`] unless an
/// explicit path override is supplied to [`UserConfig::load`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserConfig {
    data: HashMap<String, String>,
    /// Filesystem path where this config instance is loaded from and saved to.
    pub path: PathBuf,
}

impl UserConfig {
    /// Load the user config from disk.
    ///
    /// Returns an empty config when the file does not exist, cannot be read,
    /// or cannot be parsed as a JSON object. This mirrors the Python contract:
    /// missing or corrupt config files never raise at load time.
    ///
    /// When `path` is `None`, the current `ENVOY_CONFIG_ROOT` environment
    /// variable is checked at call time before falling back to
    /// `~/.envoy/user_config.json`.
    ///
    /// JSON object values are coerced into strings to match the Python port's
    /// permissive `str(value)` behavior. String values are preserved verbatim;
    /// other JSON values use a simple Rust/JSON stringification.
    pub fn load(path: Option<PathBuf>) -> Self {
        let config_path = path.unwrap_or_else(user_config_path);
        let text = match fs::read_to_string(&config_path) {
            Ok(text) => text,
            Err(_) => {
                return Self {
                    data: HashMap::new(),
                    path: config_path,
                };
            }
        };

        let value = match parse_json_with_comments::<Value>(&text) {
            Ok(value) => value,
            Err(_) => {
                return Self {
                    data: HashMap::new(),
                    path: config_path,
                };
            }
        };

        let data = match value {
            Value::Object(entries) => entries
                .into_iter()
                .map(|(key, value)| (key, stringify_json_value(&value)))
                .collect(),
            _ => HashMap::new(),
        };

        Self {
            data,
            path: config_path,
        }
    }

    /// Save the current config to disk.
    ///
    /// Parent directories are created as needed. The emitted JSON is pretty
    /// printed with sorted keys so on-disk output is deterministic and mirrors
    /// Python's `json.dumps(..., indent=2, sort_keys=True)`.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|source| EnvoyError::Io {
                    path: self.path.clone(),
                    source,
                })?;
            }
        }

        let sorted: BTreeMap<String, String> = self
            .data
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let json = serde_json::to_string_pretty(&sorted).map_err(|source| EnvoyError::Json {
            path: self.path.clone(),
            source,
        })?;

        fs::write(&self.path, json).map_err(|source| EnvoyError::Io {
            path: self.path.clone(),
            source,
        })?;

        Ok(())
    }

    /// Return the value of `key`, or `None` if it is not set.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.get(key).map(String::as_str)
    }

    /// Set `key` to `value` in memory.
    ///
    /// Call [`UserConfig::save`] to persist the change. Unknown keys and
    /// invalid choice values are reported as [`EnvoyError::Validation`].
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let setting = known_setting(key).ok_or_else(|| {
            let known = known_settings()
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ");

            EnvoyError::Validation(format!(
                "Unknown config setting {key:?}. Known settings: {known}"
            ))
        })?;

        if let Some(choices) = setting.choices {
            if !choices.contains(&value) {
                return Err(EnvoyError::Validation(format!(
                    "Invalid value {value:?} for {key:?}. Valid choices: {}",
                    choices.join(", ")
                )));
            }
        }

        self.data.insert(key.to_string(), value.to_string());
        Ok(())
    }

    /// Remove `key` from the config.
    ///
    /// Returns `true` if the key existed.
    pub fn unset(&mut self, key: &str) -> bool {
        self.data.remove(key).is_some()
    }

    /// Return a copy of all currently stored settings.
    pub fn items(&self) -> HashMap<String, String> {
        self.data.clone()
    }

    /// Return whether the config currently has no stored settings.
    ///
    /// This mirrors the false-y behavior of Python's `__bool__`.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl fmt::Display for UserConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "UserConfig(path={}, settings={})",
            self.path.display(),
            format_settings(&self.data)
        )
    }
}

#[cfg(target_os = "windows")]
fn windows_home_directory() -> PathBuf {
    non_empty_env_path("USERPROFILE")
        .or_else(windows_home_drive_path)
        .or_else(|| non_empty_env_path("HOME"))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(target_os = "windows")]
fn windows_home_drive_path() -> Option<PathBuf> {
    let mut home = env::var_os("HOMEDRIVE").filter(|value| !value.is_empty())?;
    let home_path = env::var_os("HOMEPATH").filter(|value| !value.is_empty())?;
    home.push(home_path);
    Some(PathBuf::from(home))
}

#[cfg(not(target_os = "windows"))]
fn non_windows_home_directory() -> PathBuf {
    non_empty_env_path("HOME").unwrap_or_else(|| PathBuf::from("."))
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name).and_then(|value| {
        if value.is_empty() {
            None
        } else {
            Some(PathBuf::from(value))
        }
    })
}

fn known_setting(key: &str) -> Option<&'static KnownSetting> {
    known_settings().iter().find_map(
        |(name, setting)| {
            if *name == key {
                Some(setting)
            } else {
                None
            }
        },
    )
}

fn stringify_json_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null => String::from("null"),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

fn format_settings(data: &HashMap<String, String>) -> String {
    if data.is_empty() {
        return String::from("{}");
    }

    let mut pairs: Vec<_> = data.iter().collect();
    pairs.sort_by(|left, right| left.0.cmp(right.0));

    let rendered = pairs
        .into_iter()
        .map(|(key, value)| {
            format!(
                "'{}': '{}'",
                escape_repr_string(key),
                escape_repr_string(value)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    format!("{{{rendered}}}")
}

fn escape_repr_string(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::{
        config_root, default_config_path, default_config_root, user_config_path, UserConfig,
    };
    use crate::error::EnvoyError;

    struct EnvVarGuard {
        name: &'static str,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
            let original = env::var_os(name);
            env::set_var(name, value);
            Self { name, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.original {
                env::set_var(self.name, value);
            } else {
                env::remove_var(self.name);
            }
        }
    }

    #[test]
    fn load_nonexistent_file_returns_empty_config() {
        let temp_dir = tempdir().expect("tempdir should be created");
        let path = temp_dir.path().join("missing.json");

        let config = UserConfig::load(Some(path.clone()));

        assert!(config.is_empty());
        assert_eq!(config.path, path);
    }

    #[test]
    fn load_invalid_json_returns_empty_config() {
        let temp_dir = tempdir().expect("tempdir should be created");
        let path = temp_dir.path().join("invalid.json");
        fs::write(&path, "{not valid json").expect("invalid test file should be written");

        let config = UserConfig::load(Some(path.clone()));

        assert!(config.is_empty());
        assert_eq!(config.path, path);
    }

    #[test]
    fn save_then_load_round_trips_settings() {
        let temp_dir = tempdir().expect("tempdir should be created");
        let path = temp_dir.path().join("nested").join("user_config.json");
        let mut config = UserConfig::load(Some(path.clone()));

        config
            .set("stack", "C:\\studio\\envoy\\studio.estack")
            .expect("stack should be accepted");
        config
            .set("verbosity", "verbose")
            .expect("verbosity should be accepted");
        config.save().expect("config should save");

        let reloaded = UserConfig::load(Some(path.clone()));

        assert_eq!(
            reloaded.get("stack"),
            Some("C:\\studio\\envoy\\studio.estack")
        );
        assert_eq!(reloaded.get("verbosity"), Some("verbose"));
        assert_eq!(reloaded.path, path);
    }

    #[test]
    fn load_comment_annotated_json_returns_expected_settings() {
        let temp_dir = tempdir().expect("tempdir should be created");
        let path = temp_dir.path().join("user_config.json");
        fs::write(
            &path,
            r#"{
                // default Stack
                "stack": "C:\\studio\\envoy\\studio.estack",
                "verbosity": "verbose", /* CLI default */
                # cache override
                "bundle_cache_dir": "C:\\cache"
            }"#,
        )
        .expect("commented config should be written");

        let config = UserConfig::load(Some(path.clone()));

        assert_eq!(
            config.get("stack"),
            Some("C:\\studio\\envoy\\studio.estack")
        );
        assert_eq!(config.get("verbosity"), Some("verbose"));
        assert_eq!(config.get("bundle_cache_dir"), Some("C:\\cache"));
        assert_eq!(config.path, path);
    }

    #[test]
    fn set_rejects_unknown_key() {
        let temp_dir = tempdir().expect("tempdir should be created");
        let path = temp_dir.path().join("user_config.json");
        let mut config = UserConfig::load(Some(path));

        let error = config
            .set("unknown_setting", "value")
            .expect_err("unknown key should fail validation");

        assert!(matches!(error, EnvoyError::Validation(_)));
        assert_eq!(
            error.to_string(),
            "validation error: Unknown config setting \"unknown_setting\". Known settings: \
stack, config_key_file, verbosity, bundle_cache_dir, cache"
        );
    }

    #[test]
    fn set_validates_choice_values() {
        let temp_dir = tempdir().expect("tempdir should be created");
        let path = temp_dir.path().join("user_config.json");
        let mut config = UserConfig::load(Some(path));

        let error = config
            .set("verbosity", "bogus")
            .expect_err("invalid verbosity should fail validation");

        assert!(matches!(error, EnvoyError::Validation(_)));
        assert_eq!(
            error.to_string(),
            "validation error: Invalid value \"bogus\" for \"verbosity\". Valid choices: quiet, \
normal, verbose"
        );

        config
            .set("verbosity", "quiet")
            .expect("valid verbosity should succeed");

        assert_eq!(config.get("verbosity"), Some("quiet"));
    }

    #[test]
    fn set_accepts_config_key_file() {
        let temp_dir = tempdir().expect("tempdir should be created");
        let path = temp_dir.path().join("user_config.json");
        let mut config = UserConfig::load(Some(path));

        config
            .set("config_key_file", "C:\\keys\\envoy.agekey")
            .expect("config_key_file should be accepted");

        assert_eq!(
            config.get("config_key_file"),
            Some("C:\\keys\\envoy.agekey")
        );
    }

    #[test]
    fn unset_reports_whether_key_existed() {
        let temp_dir = tempdir().expect("tempdir should be created");
        let path = temp_dir.path().join("user_config.json");
        let mut config = UserConfig::load(Some(path));
        config
            .set("stack", "studio.estack")
            .expect("stack should be accepted");

        assert!(config.unset("stack"));
        assert!(!config.unset("stack"));
    }

    #[test]
    fn default_config_path_ends_with_expected_filename() {
        let path = default_config_path();

        assert!(path.ends_with(Path::new(".envoy").join("user_config.json")));
    }

    #[test]
    fn config_root_honors_non_empty_override() {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _guard = EnvVarGuard::set("ENVOY_CONFIG_ROOT", "custom-config-root");

        assert_eq!(config_root(), Path::new("custom-config-root"));
        assert_eq!(
            user_config_path(),
            Path::new("custom-config-root").join("user_config.json")
        );
    }

    #[test]
    fn config_root_ignores_empty_override() {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _guard = EnvVarGuard::set("ENVOY_CONFIG_ROOT", "");

        assert_eq!(config_root(), default_config_root());
    }

    #[test]
    fn explicit_load_path_takes_precedence_over_config_root() {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _guard = EnvVarGuard::set("ENVOY_CONFIG_ROOT", "ignored-config-root");
        let explicit_path = PathBuf::from("explicit").join("settings.json");

        let config = UserConfig::load(Some(explicit_path.clone()));

        assert_eq!(config.path, explicit_path);
    }
}
