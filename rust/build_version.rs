//! The version a build reports, shared by the build scripts of every crate that
//! reports one.
//!
//! The version in `Cargo.toml` is the target: `0.8.0-alpha.1` while work heads
//! for 0.8.0, `0.8.0-rc.1` for a candidate, `0.8.0` for the release. It is set
//! with Prepare Release, before any tag exists.
//!
//! A build of the commit tagged `v<version>` reports exactly that. Any other
//! build adds its commit as build metadata, which never changes how versions
//! order: `v0.8.0-alpha.1+g8ec805c` is the 0.8.0-alpha.1 line built from that
//! commit, and `.dirty` follows when tracked files have uncommitted changes. A
//! build without git, from a source archive, reports the declared version alone.

use std::process::Command;

/// What a build reports as its version, given the version `Cargo.toml` declares.
pub fn reported(declared: &str) -> String {
    let commit = git(&["rev-parse", "--short=7", "HEAD"]);
    let tags = git(&["tag", "--points-at", "HEAD"]).unwrap_or_default();
    let tags: Vec<&str> = tags.lines().map(str::trim).collect();
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|changes| !changes.is_empty());
    describe(declared, commit.as_deref(), &tags, dirty)
}

/// The version for a checkout at `commit` (`None` without git) that carries
/// `tags` and has, or has not, uncommitted changes to tracked files.
pub fn describe(declared: &str, commit: Option<&str>, tags: &[&str], dirty: bool) -> String {
    let version = format!("v{declared}");
    let Some(commit) = commit else {
        return version;
    };

    let mut metadata = Vec::new();
    if !tags.contains(&version.as_str()) {
        metadata.push(format!("g{commit}"));
    }
    if dirty {
        metadata.push("dirty".to_string());
    }
    if metadata.is_empty() {
        version
    } else {
        format!("{version}+{}", metadata.join("."))
    }
}

fn git(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(arguments)
        // CARGO_MANIFEST_DIR is a crate's directory (rust/<crate>); the
        // repository root, where .git lives, is two levels up.
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_commit_tagged_with_the_version_reports_it_alone() {
        assert_eq!(
            describe("0.8.0", Some("8ec805c"), &["v0.8.0"], false),
            "v0.8.0"
        );
        assert_eq!(
            describe(
                "0.8.0-rc.1",
                Some("8ec805c"),
                &["v0.7.0", "v0.8.0-rc.1"],
                false
            ),
            "v0.8.0-rc.1",
            "a pre-release is a version like any other, and the tag may be one of several"
        );
    }

    #[test]
    fn any_other_commit_reports_the_target_and_its_own_name() {
        assert_eq!(
            describe("0.8.0-alpha.1", Some("8ec805c"), &[], false),
            "v0.8.0-alpha.1+g8ec805c"
        );
        assert_eq!(
            describe("0.8.0-alpha.1", Some("8ec805c"), &["v0.7.0"], false),
            "v0.8.0-alpha.1+g8ec805c",
            "a tag for another version does not make this build that version"
        );
    }

    #[test]
    fn uncommitted_changes_are_said_so() {
        assert_eq!(
            describe("0.8.0-alpha.1", Some("8ec805c"), &[], true),
            "v0.8.0-alpha.1+g8ec805c.dirty"
        );
        assert_eq!(
            describe("0.8.0", Some("8ec805c"), &["v0.8.0"], true),
            "v0.8.0+dirty"
        );
    }

    #[test]
    fn a_build_without_git_reports_the_declared_version_alone() {
        assert_eq!(
            describe("0.8.0-rc.1", None, &[], false),
            "v0.8.0-rc.1",
            "a source archive is the version it was cut as"
        );
    }

    #[test]
    fn this_checkout_reports_a_version_that_begins_with_the_declared_one() {
        assert!(reported("1.2.3").starts_with("v1.2.3"));
    }
}
