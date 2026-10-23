//! Semantic version parsing, validation, comparison, and constraint matching.
//!
//! This module implements semver parsing with optional `v` prefix, prerelease
//! handling per [SEMVER 2.0.0], and a simple constraint language (`>=1.0.0`,
//! `<2.0.0`, `^1.2`, `~1.2.3`, `==1.0.0`).
//!
//! One extension to SEMVER 2.0.0: the prerelease label `envoy` is reserved for
//! packaging revisions of an upstream release (`3.11.9-envoy.2` is the second
//! repackaging of 3.11.9). A revision is a release, not a prerelease: it sorts
//! after the version it repackages and counts as stable.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error type for semver operations.
#[derive(Debug, Error)]
pub enum SemVerError {
    #[error("'{0}' is not a valid semantic version. Expected MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-LABEL[.N] (e.g. 1.2.3, v1.2.3, 1.2.3-alpha, v0.0.1-alpha.3).")]
    Invalid(String),

    #[error("'{0}' is not a valid version constraint. Expected patterns like >=1.0.0, <2.0.0, ^1.2, ~1.2.3, ==1.0.0, or 1.0.0.")]
    InvalidConstraint(String),

    #[error("no version matched the constraint '{constraint}' among: {versions}")]
    NoMatch {
        constraint: String,
        versions: String,
    },
}

/// Prerelease label reserved for packaging revisions of an upstream release.
const REVISION_LABEL: &str = "envoy";

fn semver_regex() -> &'static Regex {
    use std::sync::OnceLock;
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"^v?(?P<major>0|[1-9]\d*)\.(?P<minor>0|[1-9]\d*)\.(?P<patch>0|[1-9]\d*)(?:-(?P<prerelease>[a-zA-Z][a-zA-Z0-9]*(?:\.\d+)?))?$",
        )
        .expect("semver regex must compile")
    })
}

/// Immutable semantic version with optional prerelease identifier.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SemVer {
    /// Breaking change increment.
    pub major: u64,
    /// Backwards-compatible feature increment.
    pub minor: u64,
    /// Backwards-compatible bug-fix increment.
    pub patch: u64,
    /// Optional prerelease identifier such as `alpha` or `alpha.3`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prerelease: Option<String>,
}

impl SemVer {
    /// Parse a version string with or without a leading `v`.
    pub fn parse(value: &str) -> Result<Self, SemVerError> {
        let trimmed = value.trim();
        let Some(captures) = semver_regex().captures(trimmed) else {
            return Err(SemVerError::Invalid(value.to_string()));
        };

        Ok(Self {
            major: captures["major"]
                .parse()
                .expect("regex-validated major must parse"),
            minor: captures["minor"]
                .parse()
                .expect("regex-validated minor must parse"),
            patch: captures["patch"]
                .parse()
                .expect("regex-validated patch must parse"),
            prerelease: captures
                .name("prerelease")
                .map(|value| value.as_str().to_string()),
        })
    }

    /// Return the prerelease label without the numeric suffix.
    pub fn prerelease_label(&self) -> Option<&str> {
        self.prerelease
            .as_deref()
            .map(|value| value.split('.').next().unwrap_or(value))
    }

    /// Return the numeric prerelease suffix, if present.
    pub fn prerelease_number(&self) -> Option<u64> {
        let prerelease = self.prerelease.as_deref()?;
        let (_, number) = prerelease.split_once('.')?;
        number.parse().ok()
    }

    /// Return a copy with `major` incremented and lower parts reset.
    pub fn bump_major(&self) -> Self {
        Self {
            major: self.major + 1,
            minor: 0,
            patch: 0,
            prerelease: None,
        }
    }

    /// Return a copy with `minor` incremented and lower parts reset.
    pub fn bump_minor(&self) -> Self {
        Self {
            major: self.major,
            minor: self.minor + 1,
            patch: 0,
            prerelease: None,
        }
    }

    /// Return a copy with `patch` incremented and prerelease cleared.
    pub fn bump_patch(&self) -> Self {
        Self {
            major: self.major,
            minor: self.minor,
            patch: self.patch + 1,
            prerelease: None,
        }
    }

    /// Render the version as a git tag string with a leading `v`.
    pub fn to_tag(&self) -> String {
        let base = format!("v{}.{}.{}", self.major, self.minor, self.patch);
        match &self.prerelease {
            Some(prerelease) => format!("{base}-{prerelease}"),
            None => base,
        }
    }

    /// Return `true` if this is a prerelease version.
    ///
    /// A packaging revision such as `1.2.3-envoy.2` has a prerelease tag but is
    /// a release, so it is not a prerelease; see [`SemVer::is_revision`].
    pub fn is_prerelease(&self) -> bool {
        self.prerelease.is_some() && !self.is_revision()
    }

    /// Return `true` if this is a packaging revision of an upstream release.
    ///
    /// Revisions carry the reserved prerelease label `envoy` (for example
    /// `3.11.9-envoy.2`). They sort after the version they repackage and count
    /// as stable.
    pub fn is_revision(&self) -> bool {
        self.prerelease_label() == Some(REVISION_LABEL)
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(prerelease) = &self.prerelease {
            write!(formatter, "-{prerelease}")?;
        }
        Ok(())
    }
}

impl FromStr for SemVer {
    type Err = SemVerError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
            .then_with(|| {
                compare_prerelease(self.prerelease.as_deref(), other.prerelease.as_deref())
            })
    }
}

/// Position within one `MAJOR.MINOR.PATCH`, in ascending order: prereleases,
/// the release itself, then packaging revisions of it.
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Stage<'a> {
    Prerelease { label: &'a str, number: Option<u64> },
    Release,
    Revision { number: Option<u64> },
}

fn stage(prerelease: Option<&str>) -> Stage<'_> {
    let Some(value) = prerelease else {
        return Stage::Release;
    };

    let (label, number) = split_prerelease(value);
    if label == REVISION_LABEL {
        Stage::Revision { number }
    } else {
        Stage::Prerelease { label, number }
    }
}

fn compare_prerelease(left: Option<&str>, right: Option<&str>) -> Ordering {
    stage(left).cmp(&stage(right))
}

fn split_prerelease(value: &str) -> (&str, Option<u64>) {
    match value.split_once('.') {
        Some((label, number)) => (label, number.parse().ok()),
        None => (value, None),
    }
}

// ---------------------------------------------------------------------------
// Constraint matching
// ---------------------------------------------------------------------------

/// A single version constraint such as `>=1.0.0`, `<2.0.0`, `^1.2`, etc.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Constraint {
    /// `>=version` — at least this version (prereleases excluded unless version itself is a prerelease).
    Gte(SemVer),
    /// `>version` — strictly greater than this version.
    Gt(SemVer),
    /// `<=version` — at most this version.
    Lte(SemVer),
    /// `<version` — strictly less than this version.
    Lt(SemVer),
    /// `==version` or `=version` — exact match.
    Eq(SemVer),
    /// `!=version` — not equal to this version.
    Neq(SemVer),
    /// `^major.minor` — compatible with major.minor (>=major.minor, <(major+1).0.0).
    Caret(u64, u64),
    /// `~major.minor.patch` — approximately equivalent (>=major.minor.patch, <major.(minor+1).0).
    Tilde(SemVer),
    /// Bare version string — exact match without operator prefix.
    Exact(SemVer),
}

impl Constraint {
    /// Parse a single constraint from a string like `>=1.0.0`, `^1.2`, etc.
    pub fn parse(input: &str) -> Result<Self, SemVerError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(SemVerError::InvalidConstraint(input.to_string()));
        }

        if let Some(rest) = trimmed.strip_prefix(">=") {
            Ok(Constraint::Gte(SemVer::parse(rest)?))
        } else if let Some(rest) = trimmed.strip_prefix('>') {
            Ok(Constraint::Gt(SemVer::parse(rest)?))
        } else if let Some(rest) = trimmed.strip_prefix("<=") {
            Ok(Constraint::Lte(SemVer::parse(rest)?))
        } else if let Some(rest) = trimmed.strip_prefix('<') {
            Ok(Constraint::Lt(SemVer::parse(rest)?))
        } else if let Some(rest) = trimmed.strip_prefix("==") {
            Ok(Constraint::Eq(SemVer::parse(rest)?))
        } else if let Some(rest) = trimmed.strip_prefix('!').and_then(|r| r.strip_prefix('=')) {
            Ok(Constraint::Neq(SemVer::parse(rest)?))
        } else if let Some(rest) = trimmed.strip_prefix('=') {
            // Single `=` treated as exact match.
            Ok(Constraint::Eq(SemVer::parse(rest)?))
        } else if let Some(rest) = trimmed.strip_prefix('^') {
            let parts: Vec<&str> = rest.split('.').collect();
            if parts.len() < 2 {
                return Err(SemVerError::InvalidConstraint(input.to_string()));
            }
            let major = parts[0]
                .parse::<u64>()
                .map_err(|_| SemVerError::InvalidConstraint(input.to_string()))?;
            let minor = parts[1]
                .parse::<u64>()
                .map_err(|_| SemVerError::InvalidConstraint(input.to_string()))?;
            Ok(Constraint::Caret(major, minor))
        } else if let Some(rest) = trimmed.strip_prefix('~') {
            let version = SemVer::parse(rest)?;
            Ok(Constraint::Tilde(version))
        } else {
            // Bare version — exact match.
            Ok(Constraint::Exact(SemVer::parse(trimmed)?))
        }
    }

    /// Test whether `version` satisfies this constraint.
    pub fn matches(&self, version: &SemVer) -> bool {
        match self {
            Constraint::Gte(v) => {
                if v.is_prerelease() && *v != *version {
                    return false;
                }
                version >= v
            }
            Constraint::Gt(v) => version > v,
            Constraint::Lte(v) => {
                if v.is_prerelease() && *v != *version {
                    return false;
                }
                version <= v
            }
            Constraint::Lt(v) => version < v,
            Constraint::Eq(v) => *v == *version,
            Constraint::Neq(v) => *v != *version,
            Constraint::Caret(major, minor) => {
                let lower = SemVer {
                    major: *major,
                    minor: *minor,
                    patch: 0,
                    prerelease: None,
                };
                let upper = SemVer {
                    major: major + 1,
                    minor: 0,
                    patch: 0,
                    prerelease: None,
                };
                version >= &lower && version < &upper
            }
            Constraint::Tilde(base) => {
                let lower = base.clone();
                let upper = SemVer {
                    major: base.major,
                    minor: base.minor + 1,
                    patch: 0,
                    prerelease: None,
                };
                version >= &lower && version < &upper
            }
            Constraint::Exact(v) => *v == *version,
        }
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Constraint::Gte(v) => write!(formatter, ">={v}"),
            Constraint::Gt(v) => write!(formatter, ">{v}"),
            Constraint::Lte(v) => write!(formatter, "<={v}"),
            Constraint::Lt(v) => write!(formatter, "<{v}"),
            Constraint::Eq(v) => write!(formatter, "=={v}"),
            Constraint::Neq(v) => write!(formatter, "!={v}"),
            Constraint::Caret(major, minor) => write!(formatter, "^{major}.{minor}"),
            Constraint::Tilde(v) => write!(formatter, "~{v}"),
            Constraint::Exact(v) => write!(formatter, "{v}"),
        }
    }
}

/// A version spec consisting of one or more comma-separated constraints.
/// All constraints must match for a version to satisfy the spec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionSpec {
    constraints: Vec<Constraint>,
}

impl VersionSpec {
    /// Parse a version spec string like `>=1.0.0,<2.0.0` or `^1.2`.
    pub fn parse(input: &str) -> Result<Self, SemVerError> {
        let parts: Vec<&str> = input.split(',').collect();
        if parts.is_empty() {
            return Err(SemVerError::InvalidConstraint(input.to_string()));
        }

        let constraints: Vec<Constraint> = parts
            .iter()
            .map(|part| Constraint::parse(part.trim()))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { constraints })
    }

    /// Test whether `version` satisfies all constraints in this spec.
    pub fn matches(&self, version: &SemVer) -> bool {
        self.constraints.iter().all(|c| c.matches(version))
    }

    /// Return the list of constraints.
    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }
}

impl fmt::Display for VersionSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.constraints.iter().map(|c| c.to_string()).collect();
        write!(formatter, "{}", parts.join(","))
    }
}

/// Resolve the latest version from a list that satisfies the given spec.
///
/// Returns `None` if no version matches or the list is empty. Versions are
/// sorted in descending order so the first match is the highest compatible.
pub fn resolve_latest(versions: &[SemVer], spec: &VersionSpec) -> Option<SemVer> {
    let mut sorted = versions.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a)); // descending
    sorted.into_iter().find(|v| spec.matches(v))
}

/// Resolve the latest **stable** version from a list that satisfies the given spec.
///
/// Prereleases (`1.0.0-rc.1`) are skipped; packaging revisions
/// (`3.11.9-envoy.2`) count as stable.
pub fn resolve_latest_stable(versions: &[SemVer], spec: &VersionSpec) -> Option<SemVer> {
    let mut stable: Vec<SemVer> = versions
        .iter()
        .filter(|v| !v.is_prerelease())
        .cloned()
        .collect();
    stable.sort_unstable_by(|a, b| b.cmp(a)); // descending
    stable.into_iter().find(|v| spec.matches(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_stable_and_prerelease() {
        let v = SemVer::parse("v1.2.3").expect("should parse");
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 2);
        assert_eq!(v.patch, 3);
        assert_eq!(v.prerelease, None);

        let v = SemVer::parse("0.0.1-alpha.3").expect("should parse");
        assert_eq!(v.prerelease.as_deref(), Some("alpha.3"));
    }

    #[test]
    fn parse_rejects_invalid() {
        assert!(SemVer::parse("1.2").is_err());
        assert!(SemVer::parse("").is_err());
    }

    #[test]
    fn ordering_stable_after_prerelease() {
        let a = SemVer::parse("1.0.0").unwrap();
        let b = SemVer::parse("1.0.0-alpha").unwrap();
        assert!(a > b);
    }

    #[test]
    fn ordering_places_packaging_revisions_after_their_release() {
        let ascending = [
            "1.0.0-rc.1",
            "1.0.0",
            "3.11.9-alpha.1",
            "3.11.9-rc.2",
            "3.11.9",
            "3.11.9-envoy",
            "3.11.9-envoy.1",
            "3.11.9-envoy.2",
            "3.11.9-envoy.10",
            "3.11.10-rc.1",
            "3.11.10",
        ]
        .map(|value| SemVer::parse(value).unwrap());

        for pair in ascending.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{} should sort before {}",
                pair[0],
                pair[1]
            );
        }

        let mut shuffled = ascending.iter().rev().cloned().collect::<Vec<_>>();
        shuffled.sort();
        assert_eq!(shuffled, ascending);
    }

    #[test]
    fn packaging_revision_is_stable_not_a_prerelease() {
        let revision = SemVer::parse("3.11.9-envoy.2").unwrap();
        assert!(revision.is_revision());
        assert!(!revision.is_prerelease());

        let prerelease = SemVer::parse("1.0.0-rc.1").unwrap();
        assert!(prerelease.is_prerelease());
        assert!(!prerelease.is_revision());

        let release = SemVer::parse("3.11.9").unwrap();
        assert!(!release.is_prerelease());
        assert!(!release.is_revision());
    }

    #[test]
    fn constraints_treat_a_revision_as_part_of_its_release() {
        let revision = SemVer::parse("3.11.9-envoy.1").unwrap();

        for matching in [
            ">=3.11.9",
            "^3.11",
            "~3.11.9",
            ">3.11.9",
            "<=3.11.10",
            ">=0.0.0",
        ] {
            assert!(
                Constraint::parse(matching).unwrap().matches(&revision),
                "{matching} should match {revision}"
            );
        }
        for excluded in ["<3.11.9", "<=3.11.9", "==3.11.9", "3.11.9", ">=3.11.10"] {
            assert!(
                !Constraint::parse(excluded).unwrap().matches(&revision),
                "{excluded} should not match {revision}"
            );
        }

        let bound = Constraint::parse(">=3.11.9-envoy.1").unwrap();
        assert!(bound.matches(&SemVer::parse("3.11.9-envoy.2").unwrap()));
        assert!(bound.matches(&SemVer::parse("3.12.0").unwrap()));
        assert!(!bound.matches(&SemVer::parse("3.11.9").unwrap()));
    }

    #[test]
    fn resolve_latest_stable_returns_the_newest_revision() {
        let versions = ["3.11.9", "3.11.9-envoy.1", "3.11.9-envoy.2", "3.12.0-rc.1"]
            .map(|value| SemVer::parse(value).unwrap());
        let spec = VersionSpec::parse(">=0.0.0").unwrap();

        assert_eq!(
            resolve_latest_stable(&versions, &spec).unwrap().to_string(),
            "3.11.9-envoy.2"
        );
        assert_eq!(
            resolve_latest(&versions, &spec).unwrap().to_string(),
            "3.12.0-rc.1"
        );
    }

    #[test]
    fn constraint_gte_matches() {
        let c = Constraint::parse(">=1.0.0").unwrap();
        assert!(c.matches(&SemVer::parse("1.0.0").unwrap()));
        assert!(c.matches(&SemVer::parse("2.0.0").unwrap()));
        assert!(!c.matches(&SemVer::parse("0.9.0").unwrap()));
    }

    #[test]
    fn constraint_caret_matches() {
        let c = Constraint::parse("^1.2").unwrap();
        assert!(c.matches(&SemVer::parse("1.2.0").unwrap()));
        assert!(c.matches(&SemVer::parse("1.9.9").unwrap()));
        assert!(!c.matches(&SemVer::parse("2.0.0").unwrap()));
        assert!(!c.matches(&SemVer::parse("1.1.9").unwrap()));
    }

    #[test]
    fn constraint_tilde_matches() {
        let c = Constraint::parse("~1.2.3").unwrap();
        assert!(c.matches(&SemVer::parse("1.2.3").unwrap()));
        assert!(c.matches(&SemVer::parse("1.2.9").unwrap()));
        assert!(!c.matches(&SemVer::parse("1.3.0").unwrap()));
    }

    #[test]
    fn constraint_eq_exact() {
        let c = Constraint::parse("==1.0.0").unwrap();
        assert!(c.matches(&SemVer::parse("1.0.0").unwrap()));
        assert!(!c.matches(&SemVer::parse("1.0.1").unwrap()));
    }

    #[test]
    fn version_spec_all_constraints_must_match() {
        let spec = VersionSpec::parse(">=1.0.0,<2.0.0").unwrap();
        assert!(spec.matches(&SemVer::parse("1.5.0").unwrap()));
        assert!(!spec.matches(&SemVer::parse("0.9.0").unwrap()));
        assert!(!spec.matches(&SemVer::parse("2.0.0").unwrap()));
    }

    #[test]
    fn resolve_latest_finds_highest_matching() {
        let versions = vec![
            SemVer::parse("1.0.0").unwrap(),
            SemVer::parse("1.1.0").unwrap(),
            SemVer::parse("1.2.0").unwrap(),
            SemVer::parse("2.0.0").unwrap(),
        ];
        let spec = VersionSpec::parse("<2.0.0").unwrap();
        let result = resolve_latest(&versions, &spec).unwrap();
        assert_eq!(result.to_string(), "1.2.0");
    }

    #[test]
    fn resolve_latest_stable_skips_prerelease() {
        let versions = vec![
            SemVer::parse("1.0.0").unwrap(),
            SemVer::parse("1.1.0-alpha").unwrap(),
            SemVer::parse("1.1.0").unwrap(),
        ];
        let spec = VersionSpec::parse(">=1.0.0").unwrap();
        let result = resolve_latest_stable(&versions, &spec).unwrap();
        assert_eq!(result.to_string(), "1.1.0");
    }

    #[test]
    fn resolve_latest_returns_none_when_no_match() {
        let versions = vec![SemVer::parse("0.5.0").unwrap()];
        let spec = VersionSpec::parse(">=1.0.0").unwrap();
        assert!(resolve_latest(&versions, &spec).is_none());
    }

    #[test]
    fn display_round_trips() {
        let v = SemVer::parse("v1.2.3-alpha.2").unwrap();
        assert_eq!(v.to_string(), "1.2.3-alpha.2");
        assert_eq!(v.to_tag(), "v1.2.3-alpha.2");
    }
}
