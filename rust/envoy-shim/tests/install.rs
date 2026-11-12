//! Runs the install and uninstall scripts for real, against the built launcher.
//!
//! Windows runs the PowerShell scripts and Unix the POSIX ones. Nothing here
//! touches PATH or the real home directory.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::{tempdir, TempDir};

const SHIM: &str = env!("CARGO_BIN_EXE_envoy-shim");

fn script(name: &str) -> PathBuf {
    let extension = if cfg!(windows) { "ps1" } else { "sh" };
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/shim")
        .join(format!("{name}.{extension}"))
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

#[test]
fn the_installers_a_release_publishes_are_the_scripts_in_the_repository() {
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/shim");
    for name in envoy_config::layout::INSTALLERS {
        assert!(scripts.join(name).is_file(), "{name} is not in {scripts:?}");
    }
}

/// The label the published launcher folders use for this machine.
fn target() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("windows-x86_64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x86_64-musl")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("macos-aarch64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("macos-x86_64")
    } else {
        None
    }
}

/// A home directory and a folder holding the launcher, as a release unpacks it.
struct Machine {
    _temp: TempDir,
    home: PathBuf,
    release: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let temp = tempdir().expect("tempdir is created");
        let home = temp.path().join("home");
        let release = temp.path().join("release");
        fs::create_dir_all(&home).expect("home is created");
        fs::create_dir_all(&release).expect("release is created");
        fs::copy(SHIM, release.join(exe("envoy"))).expect("launcher is copied");
        Self {
            _temp: temp,
            home,
            release,
        }
    }

    fn bin(&self) -> PathBuf {
        self.home.join(".envoy").join("bin")
    }

    fn run(&self, name: &str, arguments: &[&str]) -> Output {
        self.run_script(&script(name), arguments)
    }

    fn run_script(&self, path: &Path, arguments: &[&str]) -> Output {
        self.run_script_with(path, arguments, &[])
    }

    fn run_script_with(&self, path: &Path, arguments: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]);
            command
        } else {
            Command::new("sh")
        };
        command
            .arg(path)
            .args(arguments)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env_remove("ENVOY_CONFIG_ROOT")
            .env_remove("ENVOY_CONFIG_PROD")
            .env_remove("ENVOY_REDIRECT_CONFIG")
            .env_remove("ENVOY_CACHE_MAX_PATH")
            .envs(env.iter().copied());
        command.output().expect("the script runs")
    }

    fn install_from(&self, source: &Path) -> Output {
        self.install_from_with(source, &[])
    }

    fn install_from_with(&self, source: &Path, env: &[(&str, &str)]) -> Output {
        let mut arguments: Vec<OsString> = if cfg!(windows) {
            vec!["-NoPath".into(), "-Source".into()]
        } else {
            vec!["--source".into()]
        };
        arguments.push(source.into());
        let arguments: Vec<&str> = arguments
            .iter()
            .map(|argument| argument.to_str().expect("arguments are text"))
            .collect();
        self.run_script_with(&script("install-envoy"), &arguments, env)
    }

    fn uninstall(&self, remove_cache: bool) -> Output {
        let arguments: &[&str] = match (cfg!(windows), remove_cache) {
            (true, false) => &["-NoPath"],
            (true, true) => &["-NoPath", "-RemoveCache", "-Force"],
            (false, false) => &[],
            (false, true) => &["--remove-cache", "--yes"],
        };
        self.run("uninstall-envoy", arguments)
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn version_printed_by(program: &Path) -> String {
    let output = Command::new(program)
        .arg("--envoy-shim-version")
        .output()
        .expect("the launcher runs");
    assert!(output.status.success(), "{}", text(&output));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn the_launcher_is_installed_as_envoy_and_en() {
    let machine = Machine::new();

    let output = machine.install_from(&machine.release);
    assert!(output.status.success(), "{}", text(&output));

    let expected = version_printed_by(Path::new(SHIM));
    for name in ["envoy", "en"] {
        let installed = machine.bin().join(exe(name));
        assert!(installed.exists(), "{} is missing", installed.display());
        assert_eq!(version_printed_by(&installed), expected, "{name}");
    }
    assert_eq!(
        fs::read_dir(machine.bin()).expect("bin is listed").count(),
        2,
        "nothing is left beside the launcher"
    );
    assert!(
        text(&output).contains("ENVOY_CONFIG_PROD"),
        "it points at the next step:\n{}",
        text(&output)
    );
    // Windows says whether long paths are on; elsewhere there is nothing to say.
    assert_eq!(
        text(&output).contains("Windows long paths:"),
        cfg!(windows),
        "{}",
        text(&output)
    );

    let again = machine.install_from(&machine.release);
    assert!(again.status.success(), "{}", text(&again));
    assert_eq!(
        fs::read_dir(machine.bin()).expect("bin is listed").count(),
        2
    );
}

#[test]
fn the_installer_mentions_a_path_cap_only_while_one_is_set() {
    let machine = Machine::new();

    let plain = machine.install_from(&machine.release);
    assert!(plain.status.success(), "{}", text(&plain));
    assert!(
        !text(&plain).contains("ENVOY_CACHE_MAX_PATH"),
        "{}",
        text(&plain)
    );

    let capped = machine.install_from_with(&machine.release, &[("ENVOY_CACHE_MAX_PATH", "200")]);
    assert!(capped.status.success(), "{}", text(&capped));
    // Only Windows says anything about the limit, so only Windows has a line to correct.
    assert_eq!(
        text(&capped).contains("ENVOY_CACHE_MAX_PATH is set to 200,"),
        cfg!(windows),
        "{}",
        text(&capped)
    );

    // The installer reads it the way envoy does: blank is not set, and anything
    // but a whole number above zero is ignored.
    for blank in ["", "   "] {
        let output =
            machine.install_from_with(&machine.release, &[("ENVOY_CACHE_MAX_PATH", blank)]);
        assert!(output.status.success(), "{}", text(&output));
        assert!(
            !text(&output).contains("ENVOY_CACHE_MAX_PATH"),
            "{}",
            text(&output)
        );
    }
    for unusable in ["lots", "0"] {
        let output =
            machine.install_from_with(&machine.release, &[("ENVOY_CACHE_MAX_PATH", unusable)]);
        assert!(output.status.success(), "{}", text(&output));
        assert_eq!(
            text(&output).contains("not a whole number above zero, so envoy ignores it"),
            cfg!(windows),
            "{unusable:?}: {}",
            text(&output)
        );
    }
}

#[test]
fn an_installer_given_no_source_uses_the_folder_it_is_in() {
    let machine = Machine::new();
    let original = script("install-envoy");
    let beside_the_launcher = machine
        .release
        .join(original.file_name().expect("the script has a file name"));
    fs::copy(&original, &beside_the_launcher).expect("script is copied");
    let arguments: &[&str] = if cfg!(windows) { &["-NoPath"] } else { &[] };

    let output = machine.run_script(&beside_the_launcher, arguments);

    assert!(output.status.success(), "{}", text(&output));
    assert!(machine.bin().join(exe("envoy")).exists());
}

#[test]
fn the_launcher_the_pointer_names_is_installed_from_the_share() {
    let Some(target) = target() else { return };
    let machine = Machine::new();
    let share = machine.release.parent().expect("a parent").join("share");
    let published = share.join("v2.0.0").join(target);
    fs::create_dir_all(&published).expect("published folder is created");
    fs::copy(SHIM, published.join(exe("envoy"))).expect("launcher is published");
    fs::write(
        share.join("current.json"),
        "{\n  \"schema\": 1,\n  \"version\": \"v2.0.0\"\n}\n",
    )
    .expect("pointer is written");

    let output = machine.install_from(&share);

    assert!(output.status.success(), "{}", text(&output));
    assert!(machine.bin().join(exe("envoy")).exists());
}

#[test]
fn a_pointer_that_could_leave_the_share_is_refused() {
    let machine = Machine::new();
    let share = machine.release.parent().expect("a parent").join("share");
    fs::create_dir_all(&share).expect("share is created");
    fs::write(share.join("current.json"), "{\"version\": \"../../x\"}\n")
        .expect("pointer is written");

    let output = machine.install_from(&share);

    assert!(!output.status.success());
    assert!(
        text(&output).contains("usable version"),
        "{}",
        text(&output)
    );
    assert!(!machine.bin().join(exe("envoy")).exists());
}

#[test]
fn a_folder_without_a_launcher_is_refused_with_advice() {
    let machine = Machine::new();
    let empty = machine.release.parent().expect("a parent").join("empty");
    fs::create_dir_all(&empty).expect("empty folder is created");

    let output = machine.install_from(&empty);

    assert!(!output.status.success());
    assert!(
        text(&output).to_lowercase().contains("found neither"),
        "{}",
        text(&output)
    );
}

#[test]
fn uninstalling_removes_the_launcher_and_keeps_the_cache_unless_asked() {
    let machine = Machine::new();
    assert!(machine.install_from(&machine.release).status.success());
    let cache = machine.home.join(".envoy").join("cache").join("entry");
    fs::create_dir_all(&cache).expect("cache is created");
    fs::write(cache.join("file"), "x").expect("cache entry is written");

    let output = machine.uninstall(false);
    assert!(output.status.success(), "{}", text(&output));
    for name in ["envoy", "en"] {
        assert!(
            fs::symlink_metadata(machine.bin().join(exe(name))).is_err(),
            "{name} is still there"
        );
    }
    assert!(cache.join("file").exists(), "the cache is kept");

    let again = machine.uninstall(true);
    assert!(again.status.success(), "{}", text(&again));
    assert!(!cache.exists(), "the cache is removed when asked");
}
