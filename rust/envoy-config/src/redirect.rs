//! The redirect file: which envoy version each user runs.
//!
//! ```yaml
//! schema: 1
//! sources:                  # first that exists wins; ${VAR} expands, {version} is the pin
//!   - ${ENVOY_STUDIO_BNDLS}/envoy/{version}
//! versions:                 # group -> the exact name of a published version
//!   default: v0.7.0
//!   preview: v0.8.0-rc.1
//! users:                    # user -> group; unlisted users get `default`
//!   alice: preview
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Deserialize;
use serde_yaml::Value;

use crate::error::{ConfigError, Result};

/// The newest schema this crate understands.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// The group a user belongs to when the file does not list them.
pub const DEFAULT_GROUP: &str = "default";

/// Longest version name accepted, matching what the cache can store.
const MAX_VERSION_LENGTH: usize = 64;

#[derive(Deserialize)]
struct Raw {
    schema: Option<u32>,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    versions: BTreeMap<String, String>,
    #[serde(default)]
    users: BTreeMap<String, String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

/// A parsed redirect file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redirect {
    /// The schema the file was written with.
    pub schema: u32,
    /// Places the pinned version may be found, best first. Each holds a
    /// `{version}` placeholder and may use `${VAR}`.
    pub sources: Vec<String>,
    /// Group name to the exact name of a published version.
    pub versions: BTreeMap<String, String>,
    /// Normalized user name to group.
    users: BTreeMap<String, String>,
    /// Top-level keys this crate does not know.
    pub unknown_keys: Vec<String>,
    /// User names that were listed more than once once normalized.
    duplicate_users: Vec<String>,
}

/// Who gets which version, as decided by [`Redirect::resolve`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolution {
    /// The user, normalized.
    pub user: String,
    /// The group the user belongs to.
    pub group: String,
    /// The version that group is pinned to.
    pub version: String,
    /// `true` when the file lists the user; `false` when they got `default`.
    pub assigned: bool,
}

/// How serious a [`Problem`] is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    /// The file must not be published like this.
    Error,
    /// Worth a look, but the file still works.
    Warning,
}

/// Something [`Redirect::validate`] found wrong with a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Problem {
    /// How serious it is.
    pub severity: Severity,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(formatter, "{label}: {}", self.message)
    }
}

impl Redirect {
    /// Parse the text of a redirect file.
    ///
    /// Lenient about content: unknown keys are kept in
    /// [`unknown_keys`](Redirect::unknown_keys) and everything else is checked
    /// by [`validate`](Redirect::validate) and [`resolve`](Redirect::resolve).
    /// A file written for a newer schema is reported as such before anything
    /// else, since its other fields may mean something different.
    pub fn parse(text: &str) -> Result<Self> {
        let value: Value =
            serde_yaml::from_str(text).map_err(|error| ConfigError::Syntax(error.to_string()))?;
        let schema = match value.get("schema") {
            Some(schema) => schema
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| {
                    ConfigError::Syntax("'schema' must be a whole number".to_string())
                })?,
            None => {
                return Err(ConfigError::Syntax(
                    "there is no 'schema' field".to_string(),
                ))
            }
        };
        if schema > SUPPORTED_SCHEMA {
            return Err(ConfigError::NewerSchema {
                found: schema,
                supported: SUPPORTED_SCHEMA,
            });
        }

        let raw: Raw = serde_yaml::from_value(value)
            .map_err(|error| ConfigError::Syntax(error.to_string()))?;

        let mut users = BTreeMap::new();
        let mut duplicate_users = BTreeSet::new();
        for (user, group) in raw.users {
            let user = normalize_user(&user);
            if users
                .insert(user.clone(), group.trim().to_string())
                .is_some()
            {
                duplicate_users.insert(user);
            }
        }

        Ok(Self {
            schema: raw.schema.unwrap_or(schema),
            sources: raw.sources,
            versions: raw.versions,
            users,
            unknown_keys: raw.extra.into_keys().collect(),
            duplicate_users: duplicate_users.into_iter().collect(),
        })
    }

    /// Decide which version `user` runs.
    ///
    /// A listed user gets their group's version, and anyone else gets
    /// `default`. The version must be a safe name, since it becomes part of a
    /// path.
    pub fn resolve(&self, user: &str) -> Result<Resolution> {
        let user = normalize_user(user);
        let (group, assigned) = match self.users.get(&user) {
            Some(group) => (group.clone(), true),
            None => (DEFAULT_GROUP.to_string(), false),
        };

        let Some(version) = self.versions.get(&group) else {
            return Err(if assigned {
                ConfigError::UnknownGroup { user, group }
            } else {
                ConfigError::NoDefault { user }
            });
        };
        if !is_safe_version(version) {
            return Err(ConfigError::UnsafeVersion {
                version: version.clone(),
            });
        }

        Ok(Resolution {
            user,
            group,
            version: version.clone(),
            assigned,
        })
    }

    /// Check the file the way a publisher should: everything that would make
    /// it fail or surprise a user.
    pub fn validate(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        let mut error = |message: String| {
            problems.push(Problem {
                severity: Severity::Error,
                message,
            });
        };

        if self.sources.is_empty() {
            error("there are no 'sources', so no version can be found".to_string());
        }
        for source in &self.sources {
            if !source.contains("{version}") {
                error(format!("source '{source}' has no {{version}} placeholder"));
            }
        }

        if !self.versions.contains_key(DEFAULT_GROUP) {
            error(format!(
                "'versions' has no '{DEFAULT_GROUP}', so users who are not listed get nothing"
            ));
        }
        for (group, version) in &self.versions {
            if group.trim().is_empty() {
                error("'versions' has a group with no name".to_string());
            }
            if !is_safe_version(version) {
                error(format!(
                    "group '{group}' is pinned to '{version}', which is not a safe version name \
(use letters, digits, '.', '_', '+' and '-', at most {MAX_VERSION_LENGTH} characters)"
                ));
            }
        }

        for (user, group) in &self.users {
            if user.is_empty() {
                error("'users' has a user with no name".to_string());
            }
            if !self.versions.contains_key(group) {
                error(format!(
                    "user '{user}' is in group '{group}', which has no version"
                ));
            }
        }
        for user in &self.duplicate_users {
            error(format!("user '{user}' is listed more than once"));
        }
        for key in &self.unknown_keys {
            error(format!("unknown top-level key '{key}'"));
        }

        problems
    }
}

/// Reduce a login name to the form the file uses: no domain, lower case.
///
/// `CORP\Alice` and `alice@corp.example` both become `alice`.
pub fn normalize_user(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_domain = trimmed.rsplit('\\').next().unwrap_or(trimmed);
    let name = without_domain.split('@').next().unwrap_or(without_domain);
    name.trim().to_lowercase()
}

/// Whether `version` can safely become a directory name.
pub fn is_safe_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= MAX_VERSION_LENGTH
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
        && !version.chars().all(|c| c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    const STUDIO: &str = "\
schema: 1
sources:
  - ${ENVOY_STUDIO_BNDLS}/envoy/{version}
  - ${ENVOY_STUDIO_BNDLS}/gt/envoy/{version}
versions:
  default: v0.7.0
  preview: v0.8.0-rc.1
  frozen: v0.6.0
users:
  alice: preview
  CORP\\Bob: frozen
";

    fn studio() -> Redirect {
        Redirect::parse(STUDIO).expect("the sample parses")
    }

    #[test]
    fn a_listed_user_gets_their_groups_version_and_everyone_else_gets_default() {
        let redirect = studio();

        let alice = redirect.resolve("alice").expect("alice resolves");
        assert_eq!(
            (alice.group.as_str(), alice.version.as_str(), alice.assigned),
            ("preview", "v0.8.0-rc.1", true)
        );

        let stranger = redirect.resolve("carol").expect("carol resolves");
        assert_eq!(
            (
                stranger.group.as_str(),
                stranger.version.as_str(),
                stranger.assigned
            ),
            ("default", "v0.7.0", false)
        );
    }

    #[test]
    fn user_names_match_without_regard_to_case_or_domain() {
        let redirect = studio();

        for login in [
            "ALICE",
            "Alice",
            "corp\\alice",
            "alice@corp.example",
            " alice ",
        ] {
            assert_eq!(
                redirect.resolve(login).expect("resolves").group,
                "preview",
                "{login}"
            );
        }
        assert_eq!(
            redirect
                .resolve("bob")
                .expect("the file's own domain prefix is dropped")
                .group,
            "frozen"
        );
    }

    #[test]
    fn normalizing_a_login_drops_the_domain_and_lowers_the_case() {
        assert_eq!(normalize_user("CORP\\Alice"), "alice");
        assert_eq!(normalize_user("Alice.Smith@Corp.Example"), "alice.smith");
        assert_eq!(normalize_user("  plain "), "plain");
    }

    #[test]
    fn a_file_with_no_default_cannot_serve_an_unlisted_user() {
        let redirect = Redirect::parse(
            "schema: 1\nsources: ['/x/{version}']\nversions: {preview: v1}\nusers: {alice: preview}\n",
        )
        .expect("parses");

        assert_eq!(redirect.resolve("alice").expect("listed").version, "v1");
        assert!(matches!(
            redirect.resolve("bob"),
            Err(ConfigError::NoDefault { .. })
        ));
    }

    #[test]
    fn a_user_in_an_unknown_group_is_an_error_not_a_silent_default() {
        let redirect = Redirect::parse(
            "schema: 1\nsources: ['/x/{version}']\nversions: {default: v1}\nusers: {alice: typo}\n",
        )
        .expect("parses");

        assert!(matches!(
            redirect.resolve("alice"),
            Err(ConfigError::UnknownGroup { group, .. }) if group == "typo"
        ));
    }

    #[test]
    fn a_version_that_could_escape_its_directory_is_never_resolved() {
        for unsafe_version in ["../outside", "a/b", "..", "v1 ", ""] {
            let text = format!(
                "schema: 1\nsources: ['/x/{{version}}']\nversions: {{default: '{unsafe_version}'}}\n"
            );
            let redirect = Redirect::parse(&text).expect("parses");

            assert!(
                matches!(
                    redirect.resolve("anyone"),
                    Err(ConfigError::UnsafeVersion { .. })
                ),
                "{unsafe_version:?} must be refused"
            );
        }
    }

    #[test]
    fn a_file_for_a_newer_schema_is_reported_before_anything_else() {
        let error = Redirect::parse("schema: 2\nversions: [this, is, not, the, old, shape]\n")
            .expect_err("a newer schema is refused");

        assert!(matches!(
            error,
            ConfigError::NewerSchema {
                found: 2,
                supported: 1
            }
        ));
    }

    #[test]
    fn broken_files_say_what_is_wrong() {
        for (text, expected) in [
            ("sources: []\n", "no 'schema'"),
            ("schema: one\n", "'schema' must be a whole number"),
            ("schema: 1\nversions: [a, b]\n", "invalid type"),
            ("schema: 1\n  bad indent: [\n", ""),
        ] {
            let error = Redirect::parse(text).expect_err("the file is broken");
            assert!(
                matches!(&error, ConfigError::Syntax(message) if message.contains(expected)),
                "{text:?} gave {error}"
            );
        }
    }

    #[test]
    fn duplicate_keys_are_rejected() {
        let error = Redirect::parse("schema: 1\nschema: 1\n").expect_err("duplicates are refused");
        assert!(matches!(error, ConfigError::Syntax(_)), "{error}");
    }

    #[test]
    fn a_good_file_has_no_problems() {
        assert_eq!(studio().validate(), Vec::new());
    }

    #[test]
    fn validation_finds_everything_a_publisher_should_fix() {
        let redirect = Redirect::parse(
            "schema: 1\n\
sources: ['/no/placeholder']\n\
versions: {preview: '../up', ' ': v1}\n\
users: {alice: preview, ALICE: missing, 'dom\\\\alice': preview}\n\
surprise: true\n",
        )
        .expect("parses");

        let messages: Vec<String> = redirect
            .validate()
            .iter()
            .map(ToString::to_string)
            .collect();
        let has = |needle: &str| messages.iter().any(|message| message.contains(needle));

        assert!(has("no {version} placeholder"), "{messages:?}");
        assert!(has("has no 'default'"), "{messages:?}");
        assert!(has("not a safe version name"), "{messages:?}");
        assert!(has("a group with no name"), "{messages:?}");
        assert!(has("user 'alice' is listed more than once"), "{messages:?}");
        assert!(has("unknown top-level key 'surprise'"), "{messages:?}");
        assert!(messages.iter().all(|message| message.starts_with("error:")));
    }

    #[test]
    fn a_user_whose_group_does_not_exist_is_a_publish_error() {
        let redirect = Redirect::parse(
            "schema: 1\nsources: ['/x/{version}']\nversions: {default: v1}\nusers: {alice: missing}\n",
        )
        .expect("parses");

        assert!(redirect.validate().iter().any(|problem| problem
            .message
            .contains("group 'missing', which has no version")));
    }

    #[test]
    fn unknown_keys_are_kept_so_a_runtime_can_warn_without_failing() {
        let redirect = Redirect::parse(
            "schema: 1\nsources: ['/x/{version}']\nversions: {default: v1}\nhosts: {}\n",
        )
        .expect("parses");

        assert_eq!(redirect.unknown_keys, ["hosts"]);
        assert_eq!(
            redirect.resolve("anyone").expect("still resolves").version,
            "v1"
        );
    }

    #[test]
    fn safe_versions_are_what_the_cache_can_store() {
        for good in [
            "v0.7.0",
            "0.3.0",
            "3.11.9-envoy.2",
            "1.0.0+build.5",
            "latest",
        ] {
            assert!(is_safe_version(good), "{good}");
        }
        for bad in ["", ".", "..", "a b", "a/b", "a\\b", "é", &"x".repeat(65)] {
            assert!(!is_safe_version(bad), "{bad:?}");
        }
    }
}
