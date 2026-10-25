//! Comparing the names of versions.

/// A version name without a leading `v` and without build metadata.
///
/// Build metadata (`+g8ec805c`) says which commit built a version, not which
/// version it is, so two names that differ only in it name the same version.
pub(crate) fn release_of(version: &str) -> &str {
    let version = version.trim().trim_start_matches(['v', 'V']);
    version
        .split_once('+')
        .map_or(version, |(release, _metadata)| release)
}

/// Whether two version names are the same version, whether or not they start
/// with `v` or carry build metadata.
pub(crate) fn same_version(left: &str, right: &str) -> bool {
    release_of(left) == release_of(right)
}

/// The version an app names in answer to `--version`, such as `v0.8.0` from
/// `envoy v0.8.0`.
pub(crate) fn reported_by(answer: &str) -> Option<&str> {
    answer.lines().next()?.split_whitespace().last()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_the_same_with_or_without_a_v_or_build_metadata() {
        assert!(same_version("v0.8.0", "0.8.0"));
        assert!(same_version(" v0.8.0\n", "v0.8.0"));
        assert!(same_version("v0.8.0-rc.1+g8ec805c", "v0.8.0-rc.1"));
        assert!(same_version(
            "v0.8.0-rc.1+g8ec805c.dirty",
            "0.8.0-rc.1+g1234567"
        ));
    }

    #[test]
    fn different_versions_are_different_whatever_the_metadata() {
        assert!(!same_version("v0.8.0", "v0.8.1"));
        assert!(!same_version("v0.8.0-3-gabc", "v0.8.0"));
        assert!(
            !same_version("v0.8.0-rc.1", "v0.8.0"),
            "a candidate is not the release"
        );
        assert!(
            !same_version("v0.8.0-rc1", "v0.8.0-rc.1"),
            "the spelling counts"
        );
    }

    #[test]
    fn the_version_is_the_last_word_of_the_first_line() {
        assert_eq!(
            reported_by("envoy v0.8.0-rc.1+g8ec805c\n"),
            Some("v0.8.0-rc.1+g8ec805c")
        );
        assert_eq!(reported_by("envoy-shim 0.8.0"), Some("0.8.0"));
        assert_eq!(reported_by("v0.8.0\nsecond line"), Some("v0.8.0"));
        assert_eq!(reported_by(""), None);
    }
}
