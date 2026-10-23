use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Infer a bundle namespace from the parent directory name.
pub fn infer_namespace(bundle_root: &Path) -> String {
    let Some(parent_name) = bundle_root.parent().and_then(Path::file_name) else {
        return String::from(super::BUNDLE_DEFAULT_NAMESPACE);
    };
    let parent_name = parent_name.to_string_lossy();

    if super::bndlid::namespace_regex().is_match(&parent_name) {
        parent_name.into_owned()
    } else {
        String::from(super::BUNDLE_DEFAULT_NAMESPACE)
    }
}

pub(crate) fn resolve_input_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    normalize_windows_path(
        fs::canonicalize(&absolute).unwrap_or_else(|_| lexical_normalize(&absolute)),
    )
}

pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    normalized
}

fn normalize_windows_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy();

        if let Some(stripped) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{stripped}"));
        }

        if let Some(stripped) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }

    path
}

pub fn root_separator() -> char {
    if cfg!(windows) {
        ';'
    } else {
        ':'
    }
}

pub fn metadata_modified_timestamp(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(system_time_to_timestamp)
}

pub fn system_time_to_timestamp(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_secs())
}

pub(crate) fn current_timestamp() -> u64 {
    system_time_to_timestamp(SystemTime::now()).unwrap_or(0)
}

pub fn env_flag_enabled(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
}

pub fn json_value_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                return integer != 0;
            }
            if let Some(integer) = number.as_u64() {
                return integer != 0;
            }
            if let Some(float) = number.as_f64() {
                return float != 0.0;
            }

            false
        }
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(entries) => !entries.is_empty(),
    }
}

pub fn json_value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null => String::from("null"),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}
