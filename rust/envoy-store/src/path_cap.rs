//! A limit on the length of a path that a person sets.
//!
//! Turning on Windows long paths lets the store hold paths of about 32,000
//! characters, but a program that does not declare itself long-path aware
//! still cannot open one past 260. Where such programs run, `ENVOY_CACHE_MAX_PATH`
//! keeps the store to a lower limit while the Windows setting stays as it is.

use crate::long_paths::{group_thousands, LongPaths};
use crate::settings::CACHE_MAX_PATH_VAR;

/// What [`CACHE_MAX_PATH_VAR`] asks for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathCap {
    /// Not set, so only the machine's limit applies.
    Unset,
    /// Keep paths in the store to this many characters. Where the machine's
    /// limit is lower, that one still applies.
    Limit(usize),
    /// Set to something that is not a whole number above zero. It has no
    /// effect, and the text is kept so that it can be shown.
    Unusable(String),
}

/// The longest path the store creates, and what decided it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathLimit {
    /// Characters, counting the store's own folders.
    pub characters: usize,
    /// `true` when a [`PathCap`] set it, so that raising what the machine
    /// allows, by turning on Windows long paths, would not raise it.
    pub capped: bool,
}

impl PathCap {
    /// Read what `value` asks for. Empty counts as not set.
    pub fn parse(value: Option<&str>) -> Self {
        let Some(text) = value.map(str::trim).filter(|text| !text.is_empty()) else {
            return Self::Unset;
        };
        match text.parse::<usize>() {
            Ok(limit) if limit > 0 => Self::Limit(limit),
            _ => Self::Unusable(text.to_string()),
        }
    }

    /// Read [`CACHE_MAX_PATH_VAR`] from this process's environment.
    pub fn from_environment() -> Self {
        let value =
            std::env::var_os(CACHE_MAX_PATH_VAR).map(|value| value.to_string_lossy().into_owned());
        Self::parse(value.as_deref())
    }

    /// The limit the cap asks for, or `None` when it is not set or unusable.
    pub fn limit(&self) -> Option<usize> {
        match self {
            Self::Limit(limit) => Some(*limit),
            Self::Unset | Self::Unusable(_) => None,
        }
    }

    /// The longest path the store creates on a machine that allows what
    /// `machine` says, or `None` where nothing limits it.
    ///
    /// The lower of the two limits wins, so a cap can only lower the limit. It
    /// counts as the reason when it is as low as the machine's, because turning
    /// on Windows long paths would then change nothing.
    pub fn apply(&self, machine: LongPaths) -> Option<PathLimit> {
        match (machine.limit(), self.limit()) {
            (None, None) => None,
            (Some(characters), None) => Some(PathLimit {
                characters,
                capped: false,
            }),
            (None, Some(characters)) => Some(PathLimit {
                characters,
                capped: true,
            }),
            (Some(machine), Some(cap)) => Some(PathLimit {
                characters: machine.min(cap),
                capped: cap <= machine,
            }),
        }
    }

    /// What to tell a person about it, or `None` when it is not set.
    pub fn describe(&self, machine: LongPaths) -> Option<String> {
        match self {
            Self::Unset => None,
            Self::Unusable(text) => Some(format!(
                "{CACHE_MAX_PATH_VAR} is '{text}', which is not a whole number of characters \
above zero, so it is ignored"
            )),
            Self::Limit(cap) => Some(match machine.limit() {
                Some(machine) if *cap > machine => format!(
                    "{} characters ({CACHE_MAX_PATH_VAR} is {}, which is more than this machine \
allows, so it changes nothing)",
                    group_thousands(machine),
                    group_thousands(*cap)
                ),
                Some(machine) if *cap < machine => format!(
                    "{} characters, set by {CACHE_MAX_PATH_VAR} (without it the limit would be {})",
                    group_thousands(*cap),
                    group_thousands(machine)
                ),
                _ => format!(
                    "{} characters, set by {CACHE_MAX_PATH_VAR}",
                    group_thousands(*cap)
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PathCap, PathLimit};
    use crate::long_paths::{LongPaths, LONG_PATH_LIMIT, SHORT_PATH_LIMIT};

    #[test]
    fn the_variable_gives_a_limit_nothing_or_an_unusable_text() {
        assert_eq!(PathCap::parse(None), PathCap::Unset);
        assert_eq!(PathCap::parse(Some("")), PathCap::Unset);
        assert_eq!(PathCap::parse(Some("   ")), PathCap::Unset);
        assert_eq!(PathCap::parse(Some("200")), PathCap::Limit(200));
        assert_eq!(PathCap::parse(Some(" 200 ")), PathCap::Limit(200));
        for unusable in [
            "0",
            "-5",
            "abc",
            "2.5",
            "200 chars",
            "2,000",
            "99999999999999999999",
        ] {
            assert_eq!(
                PathCap::parse(Some(unusable)),
                PathCap::Unusable(unusable.to_string()),
                "{unusable:?}"
            );
        }
    }

    #[test]
    fn only_a_usable_cap_has_a_limit() {
        assert_eq!(PathCap::Limit(200).limit(), Some(200));
        assert_eq!(PathCap::Unset.limit(), None);
        assert_eq!(PathCap::Unusable("abc".into()).limit(), None);
    }

    fn limit(characters: usize, capped: bool) -> Option<PathLimit> {
        Some(PathLimit { characters, capped })
    }

    #[test]
    fn the_lower_of_the_machines_limit_and_the_cap_applies() {
        let cap = PathCap::Limit(200);
        assert_eq!(cap.apply(LongPaths::On), limit(200, true));
        assert_eq!(cap.apply(LongPaths::Off), limit(200, true));
        assert_eq!(cap.apply(LongPaths::Unknown), limit(200, true));
        assert_eq!(cap.apply(LongPaths::NotWindows), limit(200, true));

        let above = PathCap::Limit(5_000);
        assert_eq!(above.apply(LongPaths::On), limit(5_000, true));
        assert_eq!(
            above.apply(LongPaths::Off),
            limit(SHORT_PATH_LIMIT, false),
            "a cap cannot raise the machine's limit"
        );
    }

    #[test]
    fn a_cap_as_low_as_the_machines_limit_is_what_holds_the_limit() {
        let level = PathCap::Limit(SHORT_PATH_LIMIT);
        assert_eq!(level.apply(LongPaths::Off), limit(SHORT_PATH_LIMIT, true));
        assert_eq!(
            PathCap::Limit(LONG_PATH_LIMIT).apply(LongPaths::On),
            limit(LONG_PATH_LIMIT, true)
        );
    }

    #[test]
    fn without_a_usable_cap_the_machines_limit_stands() {
        for cap in [PathCap::Unset, PathCap::Unusable("abc".into())] {
            assert_eq!(cap.apply(LongPaths::On), limit(LONG_PATH_LIMIT, false));
            assert_eq!(cap.apply(LongPaths::Off), limit(SHORT_PATH_LIMIT, false));
            assert_eq!(cap.apply(LongPaths::NotWindows), None);
        }
    }

    #[test]
    fn it_says_what_it_does_only_when_it_is_set() {
        assert_eq!(PathCap::Unset.describe(LongPaths::On), None);

        let lower = PathCap::Limit(250)
            .describe(LongPaths::On)
            .expect("it is set");
        assert_eq!(
            lower,
            "250 characters, set by ENVOY_CACHE_MAX_PATH (without it the limit would be 32,000)"
        );

        let alone = PathCap::Limit(250)
            .describe(LongPaths::NotWindows)
            .expect("it is set");
        assert_eq!(alone, "250 characters, set by ENVOY_CACHE_MAX_PATH");
        let level = PathCap::Limit(250)
            .describe(LongPaths::Off)
            .expect("it is set");
        assert_eq!(level, alone);

        let above = PathCap::Limit(5_000)
            .describe(LongPaths::Off)
            .expect("it is set");
        assert!(
            above.starts_with("250 characters (ENVOY_CACHE_MAX_PATH is 5,000"),
            "{above}"
        );
        assert!(above.ends_with("so it changes nothing)"), "{above}");

        let unusable = PathCap::Unusable("2OO".into())
            .describe(LongPaths::On)
            .expect("it is set");
        assert!(
            unusable.starts_with("ENVOY_CACHE_MAX_PATH is '2OO'"),
            "{unusable}"
        );
        assert!(unusable.ends_with("so it is ignored"), "{unusable}");
    }
}
