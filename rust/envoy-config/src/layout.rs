//! Where the launcher and the app live, and what they are called.
//!
//! The launcher reads these places and the tools that publish to them write
//! them, so the names are fixed here, once, and both sides use them.
//!
//! On the share, under the config root (see [`locate`](crate::locate)):
//!
//! ```text
//! shim/
//!   current.json                      the launcher version installed launchers follow
//!   install-envoy.ps1, ...            the installers, which move with the pointer
//!   <version>/<target>/envoy[.exe]    the launcher for each platform, with its installers
//! ```
//!
//! A published app is a folder with its program at `bin/envoy[.exe]`.

use std::fmt;

/// The folder under the config root that holds the launcher.
pub const SHIM_DIR: &str = "shim";

/// The file in [`SHIM_DIR`] that names the launcher version installed launchers follow.
pub const POINTER_FILE: &str = "current.json";

/// The newest layout of the pointer file that a launcher understands and a tool writes.
pub const POINTER_SCHEMA: u32 = 1;

/// What the name of a launcher's archive starts with:
/// `envoy-shim-<version>-<target>.<extension>`.
pub const ARCHIVE_PREFIX: &str = "envoy-shim-";

/// What `--envoy-shim-version` prints before the version.
pub const VERSION_REPORT_PREFIX: &str = "envoy-shim ";

/// The installers that go up to the shim folder, those for Windows first.
pub const INSTALLERS: [&str; 4] = [
    "install-envoy.ps1",
    "uninstall-envoy.ps1",
    "install-envoy.sh",
    "uninstall-envoy.sh",
];

/// The file the launcher is published as, in its folder: `envoy.exe` on Windows
/// and `envoy` elsewhere.
pub const fn launcher_file(windows: bool) -> &'static str {
    if windows {
        "envoy.exe"
    } else {
        "envoy"
    }
}

/// Where a published app's program is, relative to the app's folder, with `/`
/// between the parts: `bin/envoy.exe` on Windows and `bin/envoy` elsewhere.
pub const fn app_executable(windows: bool) -> &'static str {
    if windows {
        "bin/envoy.exe"
    } else {
        "bin/envoy"
    }
}

/// A platform the launcher is built for, named as the folders of
/// `shim/<version>/` name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    /// `windows-x86_64`.
    WindowsX64,
    /// `linux-x86_64-musl`.
    LinuxX64Musl,
    /// `macos-aarch64`.
    MacosArm64,
    /// `macos-x86_64`.
    MacosX64,
}

impl Target {
    /// Every platform a release builds the launcher for.
    pub const ALL: [Target; 4] = [
        Target::WindowsX64,
        Target::LinuxX64Musl,
        Target::MacosArm64,
        Target::MacosX64,
    ];

    /// The name of the platform's folder, and of its part of an archive's name.
    pub fn label(self) -> &'static str {
        match self {
            Self::WindowsX64 => "windows-x86_64",
            Self::LinuxX64Musl => "linux-x86_64-musl",
            Self::MacosArm64 => "macos-aarch64",
            Self::MacosX64 => "macos-x86_64",
        }
    }

    /// The platform a folder or an archive is named for.
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|target| target.label() == label)
    }

    /// The platform this program runs on, when a launcher is built for it.
    pub fn host() -> Option<Self> {
        if cfg!(all(windows, target_arch = "x86_64")) {
            Some(Self::WindowsX64)
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Self::LinuxX64Musl)
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some(Self::MacosArm64)
        } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            Some(Self::MacosX64)
        } else {
            None
        }
    }

    /// Whether the platform is Windows.
    pub fn is_windows(self) -> bool {
        self == Self::WindowsX64
    }

    /// Windows archives are ZIP files; the others are compressed tar files.
    pub fn archive_extension(self) -> &'static str {
        if self.is_windows() {
            "zip"
        } else {
            "tar.gz"
        }
    }

    /// The name of this platform's launcher archive for `version`, which is
    /// written as the release's tag is, such as `v0.8.0`.
    pub fn archive_name(self, version: &str) -> String {
        format!(
            "{ARCHIVE_PREFIX}{version}-{}.{}",
            self.label(),
            self.archive_extension()
        )
    }

    /// The platform and the version, as written in the name, that a launcher
    /// archive is named for. `None` for any other file name, such as an app's
    /// archive, or for an archive whose extension is not its platform's.
    ///
    /// The version is not checked.
    pub fn split_archive_name(name: &str) -> Option<(Self, &str)> {
        let rest = name.strip_prefix(ARCHIVE_PREFIX)?;
        Self::ALL.into_iter().find_map(|target| {
            let suffix = format!("-{}.{}", target.label(), target.archive_extension());
            let version = rest.strip_suffix(suffix.as_str())?;
            (!version.is_empty()).then_some((target, version))
        })
    }

    /// The file the launcher is published as.
    pub fn launcher(self) -> &'static str {
        launcher_file(self.is_windows())
    }

    /// Where a published app's program is on this platform.
    pub fn app_executable(self) -> &'static str {
        app_executable(self.is_windows())
    }

    /// The installer and the uninstaller an archive for this platform carries.
    pub fn installers(self) -> [&'static str; 2] {
        if self.is_windows() {
            [INSTALLERS[0], INSTALLERS[1]]
        } else {
            [INSTALLERS[2], INSTALLERS[3]]
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_names_its_launcher_archives_this_way() {
        let names: Vec<String> = Target::ALL
            .into_iter()
            .map(|target| target.archive_name("v0.8.0"))
            .collect();

        assert_eq!(
            names,
            [
                "envoy-shim-v0.8.0-windows-x86_64.zip",
                "envoy-shim-v0.8.0-linux-x86_64-musl.tar.gz",
                "envoy-shim-v0.8.0-macos-aarch64.tar.gz",
                "envoy-shim-v0.8.0-macos-x86_64.tar.gz",
            ]
        );
    }

    #[test]
    fn an_archive_name_gives_back_its_platform_and_version() {
        for target in Target::ALL {
            for version in ["v0.8.0", "v0.8.0-rc.1", "0.8.0-alpha.3"] {
                let name = target.archive_name(version);

                assert_eq!(
                    Target::split_archive_name(&name),
                    Some((target, version)),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn only_a_launcher_archive_of_a_platform_is_one() {
        for name in [
            "envoy-v0.8.0-windows-x86_64.zip",
            "envoy-shim-v0.8.0-windows-x86_64.tar.gz",
            "envoy-shim-v0.8.0-linux-x86_64-musl.zip",
            "envoy-shim-v0.8.0-windows-aarch64.zip",
            "envoy-shim-v0.8.0-macos-x86_64.tar.gz.sig",
            "envoy-shim--windows-x86_64.zip",
            "envoy-shim-windows-x86_64.zip",
            "SHA256SUMS",
            "",
        ] {
            assert_eq!(Target::split_archive_name(name), None, "{name:?}");
        }
    }

    #[test]
    fn a_label_names_one_platform() {
        for target in Target::ALL {
            assert_eq!(Target::from_label(target.label()), Some(target));
            assert_eq!(target.to_string(), target.label());
        }
        assert_eq!(Target::from_label("linux-x86_64"), None);
        assert_eq!(Target::from_label("Windows-x86_64"), None);
    }

    #[test]
    fn windows_has_its_own_names() {
        assert_eq!(Target::WindowsX64.launcher(), "envoy.exe");
        assert_eq!(Target::WindowsX64.app_executable(), "bin/envoy.exe");
        assert_eq!(
            Target::WindowsX64.installers(),
            ["install-envoy.ps1", "uninstall-envoy.ps1"]
        );
        for target in [Target::LinuxX64Musl, Target::MacosArm64, Target::MacosX64] {
            assert_eq!(target.launcher(), "envoy");
            assert_eq!(target.app_executable(), "bin/envoy");
            assert_eq!(
                target.installers(),
                ["install-envoy.sh", "uninstall-envoy.sh"]
            );
        }
    }

    #[test]
    fn every_installer_belongs_to_a_platform() {
        let mut named: Vec<&str> = Target::ALL
            .into_iter()
            .flat_map(Target::installers)
            .collect();
        named.sort_unstable();
        named.dedup();

        let mut all = INSTALLERS.to_vec();
        all.sort_unstable();
        assert_eq!(named, all);
    }

    #[test]
    fn this_machine_is_a_platform_a_release_is_built_for() {
        let built_for = cfg!(any(
            all(windows, target_arch = "x86_64"),
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "macos", target_arch = "x86_64"),
        ));

        assert_eq!(Target::host().is_some(), built_for);
        if let Some(host) = Target::host() {
            assert_eq!(host.is_windows(), cfg!(windows));
        }
    }
}
