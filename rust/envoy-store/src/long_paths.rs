//! Whether Windows lets programs use paths longer than 260 characters.
//!
//! Windows ships with a limit of 260 characters on a path, and many programs
//! still honour it. Since Windows 10 version 1607 an administrator can turn on
//! long paths (the `LongPathsEnabled` registry value, which the "Enable Win32
//! long paths" group policy also sets). A program that declares itself
//! long-path aware may then use paths of about 32,000 characters. The store
//! keeps what it caches within what the machine allows, so a bundle with deep
//! folders fits on a machine that has long paths turned on and runs from its
//! source on one that does not.

use std::sync::OnceLock;

/// The longest path the store creates while long paths are off. It leaves room
/// below the classic limit of 260 that many programs still honour.
pub const SHORT_PATH_LIMIT: usize = 250;

/// The longest path the store creates while long paths are on. Windows allows
/// 32,767 characters, and some room is left for what a program adds.
pub const LONG_PATH_LIMIT: usize = 32_000;

/// What this machine allows for the length of a path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LongPaths {
    /// Windows with long paths turned on.
    On,
    /// Windows with long paths off, the way it ships.
    Off,
    /// Windows, but the setting could not be read. It is treated as off.
    Unknown,
    /// Not Windows, which has no such limit.
    NotWindows,
}

impl LongPaths {
    /// Read the machine's setting.
    ///
    /// It is read once per process, as Windows reads it, so a change shows only
    /// in programs started afterwards.
    pub fn detect() -> Self {
        static DETECTED: OnceLock<LongPaths> = OnceLock::new();
        *DETECTED.get_or_init(read_machine_setting)
    }

    /// The longest path the store creates, or `None` where there is no limit to
    /// keep to.
    pub fn limit(self) -> Option<usize> {
        match self {
            Self::On => Some(LONG_PATH_LIMIT),
            Self::Off | Self::Unknown => Some(SHORT_PATH_LIMIT),
            Self::NotWindows => None,
        }
    }

    /// `true` when turning on Windows long paths would raise the limit.
    pub fn can_be_raised(self) -> bool {
        matches!(self, Self::Off | Self::Unknown)
    }

    /// What to tell a person about it, or `None` where there is nothing to say.
    pub fn describe(self) -> Option<String> {
        match self {
            Self::On => Some(format!(
                "on (paths in the cache may reach about {} characters; a program must declare \
itself long-path aware to open them)",
                group_thousands(LONG_PATH_LIMIT)
            )),
            Self::Off => Some(format!(
                "off (paths in the cache are limited to {SHORT_PATH_LIMIT} characters; an \
administrator can turn Windows long paths on to raise that)"
            )),
            Self::Unknown => Some(format!(
                "unknown (the Windows setting could not be read, so paths in the cache are \
limited to {SHORT_PATH_LIMIT} characters as if it were off)"
            )),
            Self::NotWindows => None,
        }
    }
}

/// What reading `LongPathsEnabled` found: its value, nothing when the value is
/// not there, or the error code Windows gave.
#[cfg(any(windows, test))]
type Reading = Result<Option<u32>, u32>;

/// Turn what was read into the setting. A missing value is the default, which
/// is off, and only a value of 1 turns long paths on.
#[cfg(any(windows, test))]
fn classify(reading: Reading) -> LongPaths {
    match reading {
        Ok(Some(1)) => LongPaths::On,
        Ok(_) => LongPaths::Off,
        Err(_) => LongPaths::Unknown,
    }
}

pub(crate) fn group_thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(not(windows))]
fn read_machine_setting() -> LongPaths {
    LongPaths::NotWindows
}

/// Where Windows keeps the setting. The "Enable Win32 long paths" group policy
/// writes the same value.
#[cfg(windows)]
const LONG_PATHS_KEY: &str = r"SYSTEM\CurrentControlSet\Control\FileSystem";

#[cfg(windows)]
const LONG_PATHS_VALUE: &str = "LongPathsEnabled";

#[cfg(windows)]
fn read_machine_setting() -> LongPaths {
    read_setting_with(read_machine_dword)
}

/// Read the setting through `read`, which is given the key and the name of the
/// value, in that order.
#[cfg(windows)]
fn read_setting_with(read: impl FnOnce(&str, &str) -> Reading) -> LongPaths {
    classify(read(LONG_PATHS_KEY, LONG_PATHS_VALUE))
}

/// Read a DWORD value from under `HKEY_LOCAL_MACHINE`.
#[cfg(windows)]
fn read_machine_dword(subkey: &str, name: &str) -> Reading {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD,
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let subkey = wide(subkey);
    let name = wide(name);
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;

    // SAFETY: `subkey` and `name` are NUL-terminated and outlive the call. The
    // type filter limits the data to a DWORD, and `value` is a DWORD whose size
    // is passed in `size`. The type output is not wanted, so it is null.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(value).cast(),
            &mut size,
        )
    };

    match status {
        ERROR_SUCCESS => Ok(Some(value)),
        ERROR_FILE_NOT_FOUND => Ok(None),
        other => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, group_thousands, LongPaths, LONG_PATH_LIMIT, SHORT_PATH_LIMIT};

    #[test]
    fn only_a_value_of_one_turns_long_paths_on() {
        assert_eq!(classify(Ok(Some(1))), LongPaths::On);
        assert_eq!(classify(Ok(Some(0))), LongPaths::Off);
        assert_eq!(classify(Ok(Some(2))), LongPaths::Off);
    }

    #[test]
    fn a_missing_value_is_the_default_and_a_failed_read_is_unknown() {
        assert_eq!(classify(Ok(None)), LongPaths::Off);
        assert_eq!(classify(Err(5)), LongPaths::Unknown);
    }

    #[test]
    fn the_limit_follows_the_setting() {
        assert_eq!(LongPaths::On.limit(), Some(LONG_PATH_LIMIT));
        assert_eq!(LongPaths::Off.limit(), Some(SHORT_PATH_LIMIT));
        assert_eq!(LongPaths::Unknown.limit(), Some(SHORT_PATH_LIMIT));
        assert_eq!(LongPaths::NotWindows.limit(), None);
    }

    #[test]
    fn only_a_machine_with_long_paths_off_could_raise_the_limit() {
        assert!(LongPaths::Off.can_be_raised());
        assert!(LongPaths::Unknown.can_be_raised());
        assert!(!LongPaths::On.can_be_raised());
        assert!(!LongPaths::NotWindows.can_be_raised());
    }

    #[test]
    fn a_report_names_the_state_and_the_limit_that_applies() {
        let on = LongPaths::On.describe().expect("windows reports");
        assert!(on.starts_with("on ") && on.contains("32,000"), "{on}");
        let off = LongPaths::Off.describe().expect("windows reports");
        assert!(off.starts_with("off ") && off.contains("250"), "{off}");
        let unknown = LongPaths::Unknown.describe().expect("windows reports");
        assert!(unknown.starts_with("unknown ") && unknown.contains("250"));
        assert!(LongPaths::NotWindows.describe().is_none());
    }

    #[test]
    fn numbers_are_grouped_in_thousands() {
        assert_eq!(group_thousands(32_000), "32,000");
        assert_eq!(group_thousands(250), "250");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_machine_answers_and_is_never_not_windows() {
        assert_ne!(LongPaths::detect(), LongPaths::NotWindows);
    }

    // The registry read is tried against a key that every Windows has and any
    // user can read, so that it is checked whatever this machine's setting is.
    #[cfg(windows)]
    const VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

    #[cfg(windows)]
    #[test]
    fn a_dword_is_read_from_the_registry() {
        let major = super::read_machine_dword(VERSION_KEY, "CurrentMajorVersionNumber");

        assert!(
            matches!(major, Ok(Some(number)) if number >= 10),
            "{major:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_value_that_is_not_there_is_not_an_error() {
        let missing = super::read_machine_dword(VERSION_KEY, "EnvoyNoSuchValue");

        assert_eq!(missing, Ok(None));
    }

    #[cfg(windows)]
    #[test]
    fn a_value_of_another_type_is_an_error() {
        // The product name is text.
        let text = super::read_machine_dword(VERSION_KEY, "ProductName");

        assert!(text.is_err(), "{text:?}");
    }

    /// The text of an attribute in the opening tag of an element of an ADMX file,
    /// quoted either way, with any whitespace around the `=`.
    #[cfg(windows)]
    fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        let mut rest = tag;
        while let Some(at) = rest.find(name) {
            let after = &rest[at + name.len()..];
            if rest[..at].ends_with(char::is_whitespace) {
                if let Some(value) = after.trim_start().strip_prefix('=') {
                    let value = value.trim_start();
                    let quote = value.chars().next().filter(|c| matches!(c, '"' | '\''))?;
                    return value[1..].split(quote).next();
                }
            }
            rest = after;
        }
        None
    }

    /// The opening tag of the policy called `name` in an ADMX file.
    #[cfg(windows)]
    fn policy_tag<'a>(admx: &'a str, name: &str) -> Option<&'a str> {
        admx.split("<policy")
            .skip(1)
            .filter_map(|element| element.split('>').next())
            .find(|tag| attribute(tag, "name") == Some(name))
    }

    // Which key and value are read is not something a test of the read itself
    // can show: a typo reads a value that is not there, which is the default.
    // Windows ships the definition of the policy that sets the value.
    #[cfg(windows)]
    #[test]
    fn the_registry_names_are_the_ones_the_policy_that_sets_them_uses() {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let admx = std::path::Path::new(&root).join(r"PolicyDefinitions\FileSys.admx");
        let Ok(text) = std::fs::read_to_string(&admx) else {
            // Not every Windows image has the policy definitions.
            return;
        };
        let Some(tag) = policy_tag(&text, "LongPathsEnabled") else {
            // A Windows from before the policy existed has the file without it.
            return;
        };

        let key = attribute(tag, "key").expect("the policy names a key");
        assert!(key.eq_ignore_ascii_case(super::LONG_PATHS_KEY), "{tag}");
        assert_eq!(
            attribute(tag, "valueName"),
            Some(super::LONG_PATHS_VALUE),
            "{tag}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_policy_tag_is_found_however_it_is_written() {
        let admx = "<policyDefinitions revision='1.0'>\n<policies>\n\
<policy name=\"Other\" class=\"Machine\" key=\"Wrong\" valueName=\"Wrong\">\n</policy>\n\
<policy\n\tclass=\"Machine\"\n\tkey = 'SYSTEM\\Right'\n\tname='LongPathsEnabled'\n\
\tvalueName = \"Right\" >\n</policy></policies></policyDefinitions>";

        let tag = policy_tag(admx, "LongPathsEnabled").expect("the policy is found");

        assert_eq!(attribute(tag, "key"), Some(r"SYSTEM\Right"));
        assert_eq!(attribute(tag, "valueName"), Some("Right"));
        assert_eq!(policy_tag(admx, "NotThere"), None);
    }

    // Which key and value the code reads, and in which order, is also not
    // something the machine's own setting can show: with the two swapped, both
    // read a value that is not there.
    #[cfg(windows)]
    #[test]
    fn the_setting_is_read_from_the_policys_key_and_value_in_that_order() {
        let mut asked = None;

        let setting = super::read_setting_with(|key, name| {
            asked = Some((key.to_string(), name.to_string()));
            Ok(Some(1))
        });

        assert_eq!(setting, LongPaths::On);
        assert_eq!(
            asked,
            Some((
                super::LONG_PATHS_KEY.to_string(),
                super::LONG_PATHS_VALUE.to_string()
            ))
        );
    }

    // The same value read by another program, on a machine where it is set this
    // shows a read of the wrong place, and where it is not it shows the default.
    // The names are written out here, so that a mistake in the ones the code
    // reads is not repeated.
    #[cfg(windows)]
    #[test]
    fn the_machines_setting_is_read_the_way_reg_query_reports_it() {
        let output = std::process::Command::new("reg.exe")
            .args([
                "query",
                r"HKLM\SYSTEM\CurrentControlSet\Control\FileSystem",
                "/v",
                "LongPathsEnabled",
            ])
            .output()
            .expect("reg.exe runs");
        let text = String::from_utf8_lossy(&output.stdout);
        let reported: Option<Vec<&str>> = text
            .lines()
            .find(|line| line.contains("LongPathsEnabled"))
            .map(|line| line.split_whitespace().collect());

        let expected = match reported.as_deref() {
            None => LongPaths::Off,
            Some([_, "REG_DWORD", "0x1"]) => LongPaths::On,
            Some([_, "REG_DWORD", _]) => LongPaths::Off,
            Some(_) => LongPaths::Unknown,
        };

        assert_eq!(super::read_machine_setting(), expected, "{text}");
    }

    #[cfg(not(windows))]
    #[test]
    fn other_systems_have_no_limit() {
        assert_eq!(LongPaths::detect(), LongPaths::NotWindows);
    }
}
