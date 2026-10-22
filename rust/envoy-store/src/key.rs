use std::fmt;
use std::path::PathBuf;

use crate::error::{Result, StoreError};

const MAX_COMPONENT_LENGTH: usize = 64;

/// Identity of one cached bundle: its `namespace:name` and the exact version
/// string its `.bundle` marker states.
///
/// The version is kept verbatim, so `v0.7.0` and `0.7.0` are different
/// entries. Every part becomes a directory name, so each is validated to
/// contain only characters that are safe on every platform.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct EntryKey {
    namespace: String,
    name: String,
    version: String,
}

impl EntryKey {
    /// Validate and build a key.
    pub fn new(namespace: &str, name: &str, version: &str) -> Result<Self> {
        check("namespace", namespace, |c| {
            c.is_ascii_alphanumeric() || c == '_' || c == '-'
        })?;
        check("name", name, |c| {
            c.is_ascii_alphanumeric() || c == '_' || c == '-'
        })?;
        check("version", version, |c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')
        })?;
        if version.chars().all(|c| c == '.') {
            return Err(StoreError::InvalidKey {
                part: "version",
                value: version.to_string(),
                reason: "must not be made only of dots",
            });
        }

        Ok(Self {
            namespace: namespace.to_string(),
            name: name.to_string(),
            version: version.to_string(),
        })
    }

    /// Return the namespace.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Return the bundle name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the exact version string.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Return the entry's directory relative to the store root.
    pub(crate) fn relative_dir(&self) -> PathBuf {
        PathBuf::from("bundles")
            .join(&self.namespace)
            .join(&self.name)
            .join(&self.version)
    }

    /// Return the file name of this entry's lock.
    ///
    /// `~` cannot appear in any part, so two keys never share a lock name.
    pub(crate) fn lock_file_name(&self) -> String {
        format!("{}~{}~{}.lock", self.namespace, self.name, self.version)
    }

    /// Return the file name of this entry's refusal record.
    pub(crate) fn refusal_file_name(&self) -> String {
        format!("{}~{}~{}.json", self.namespace, self.name, self.version)
    }
}

impl fmt::Display for EntryKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}@{}",
            self.namespace, self.name, self.version
        )
    }
}

fn check(part: &'static str, value: &str, allowed: impl Fn(char) -> bool) -> Result<()> {
    let reason = if value.is_empty() {
        Some("must not be empty")
    } else if value.len() > MAX_COMPONENT_LENGTH {
        Some("is too long")
    } else if !value.chars().all(allowed) {
        Some("contains a character that is not allowed in a store path")
    } else {
        None
    };

    match reason {
        Some(reason) => Err(StoreError::InvalidKey {
            part,
            value: value.to_string(),
            reason,
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::EntryKey;

    #[test]
    fn keeps_the_version_verbatim() {
        let with_prefix = EntryKey::new("gt", "envoy", "v0.7.0").unwrap();
        let without = EntryKey::new("gt", "envoy", "0.7.0").unwrap();

        assert_ne!(with_prefix, without);
        assert_eq!(with_prefix.to_string(), "gt:envoy@v0.7.0");
    }

    #[test]
    fn accepts_revision_and_build_versions() {
        assert!(EntryKey::new("ext", "python", "3.11.9-envoy.2").is_ok());
        assert!(EntryKey::new("ext", "pyside", "6.11.1-envoy.1+build.5").is_ok());
    }

    #[test]
    fn rejects_parts_that_are_not_safe_path_components() {
        for (namespace, name, version) in [
            ("gt", "envoy", ""),
            ("gt", "envoy", ".."),
            ("gt", "envoy", "1.0/../x"),
            ("gt", "envoy", r"1.0\0"),
            ("gt", "en voy", "1.0.0"),
            ("g:t", "envoy", "1.0.0"),
            ("gt", "", "1.0.0"),
            ("", "envoy", "1.0.0"),
        ] {
            assert!(
                EntryKey::new(namespace, name, version).is_err(),
                "{namespace}:{name}@{version} should be rejected"
            );
        }
    }

    #[test]
    fn lock_names_are_distinct_for_distinct_keys() {
        let first = EntryKey::new("a_", "b", "1").unwrap();
        let second = EntryKey::new("a", "_b", "1").unwrap();

        assert_ne!(first.lock_file_name(), second.lock_file_name());
    }
}
