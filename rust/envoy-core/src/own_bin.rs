//! Keeps `envoy` run from inside an environment on the version that started it.
//!
//! A stack can list an envoy bundle of its own, and that bundle's `bin` then sits
//! on `PATH` ahead of everything else. But the version a person runs is chosen by
//! the launcher, so a program that envoy starts and that runs `envoy` or `en`
//! again must reach the envoy that started it, not the one the stack happens to
//! pin. When envoy runs from a release bundle, its own `bin` goes first on `PATH`
//! for the commands it launches.
//!
//! Set [`SWITCH_VAR`] to `off` to leave `PATH` alone.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::bundle_store::parse_switch;
use crate::python_api::same_path;

/// `off`, `false`, `no` or `0` stops envoy putting its own `bin` on `PATH`.
pub const SWITCH_VAR: &str = "ENVOY_OWN_BIN";

const PATH: &str = "PATH";

/// Return the `bin` directory that holds `exe`, when `exe` is envoy's own
/// executable in a release bundle, which is marked by a `.bundle` file beside
/// `bin`.
///
/// Anything else, such as a development build, has no `bin` of its own to put
/// first: its directory holds build products, not a bundle.
pub fn bundled_bin(exe: &Path) -> Option<PathBuf> {
    let stem = exe.file_stem()?.to_str()?;
    if !stem.eq_ignore_ascii_case("envoy") {
        return None;
    }

    let bin = exe.parent()?;
    bin.parent()?
        .join(".bundle")
        .is_file()
        .then(|| bin.to_path_buf())
}

/// Put `bin` first on the `PATH` of `env`, taking it from wherever else it was
/// listed.
pub fn add_to_env(env: &mut HashMap<String, String>, bin: &Path) {
    // Windows spells the variable as it likes, and a second spelling would hide the first.
    let key = env
        .keys()
        .find(|name| name.eq_ignore_ascii_case(PATH))
        .cloned()
        .unwrap_or_else(|| PATH.to_string());

    let mut entries: Vec<PathBuf> = env
        .get(&key)
        .map(|existing| std::env::split_paths(&OsString::from(existing)).collect())
        .unwrap_or_default();
    entries.retain(|entry| !entry.as_os_str().is_empty() && !same_path(entry, bin));
    entries.insert(0, bin.to_path_buf());

    if let Ok(joined) = std::env::join_paths(&entries) {
        env.insert(key, joined.to_string_lossy().into_owned());
    }
}

/// Put the running envoy's `bin` first on the `PATH` of `env`, unless that is
/// switched off or envoy is not running from a release bundle.
///
/// Returns the directory that was added.
pub fn apply(env: &mut HashMap<String, String>) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let switch = std::env::var(SWITCH_VAR).ok();
    apply_for(env, &exe, switch.as_deref())
}

fn apply_for(
    env: &mut HashMap<String, String>,
    exe: &Path,
    switch: Option<&str>,
) -> Option<PathBuf> {
    if switch.and_then(parse_switch) == Some(false) {
        return None;
    }

    let bin = bundled_bin(exe)?;
    add_to_env(env, &bin);
    Some(bin)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn release(root: &Path) -> PathBuf {
        fs::create_dir_all(root.join("bin")).expect("bin is created");
        fs::write(root.join(".bundle"), "{}").expect("marker is written");
        let exe = root
            .join("bin")
            .join(if cfg!(windows) { "envoy.exe" } else { "envoy" });
        fs::write(&exe, "").expect("exe is written");
        exe
    }

    fn joined(entries: &[&str]) -> String {
        std::env::join_paths(entries)
            .expect("entries join")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn a_release_bundle_has_its_bin_beside_its_marker() {
        let temp = tempdir().expect("tempdir is created");
        let exe = release(temp.path());

        assert_eq!(bundled_bin(&exe), Some(temp.path().join("bin")));
    }

    #[test]
    fn only_envoys_own_release_layout_counts() {
        let temp = tempdir().expect("tempdir is created");
        let exe = release(temp.path());

        let python = temp.path().join("bin").join("python.exe");
        assert_eq!(bundled_bin(&python), None, "not envoy's executable");

        fs::remove_file(temp.path().join(".bundle")).expect("marker is removed");
        assert_eq!(bundled_bin(&exe), None, "a build directory is not a bundle");
    }

    #[test]
    fn a_release_puts_its_bin_first_unless_the_switch_is_off() {
        let temp = tempdir().expect("tempdir is created");
        let exe = release(temp.path());
        let bin = temp.path().join("bin");

        for switch in [None, Some("on"), Some("1"), Some("not a switch")] {
            let mut env = HashMap::new();
            assert_eq!(apply_for(&mut env, &exe, switch), Some(bin.clone()));
            assert_eq!(env["PATH"], bin.display().to_string());
        }

        for switch in ["off", "OFF", "0", "false", "no"] {
            let mut env = HashMap::new();
            assert_eq!(apply_for(&mut env, &exe, Some(switch)), None, "{switch}");
            assert!(env.is_empty(), "{switch} leaves the environment alone");
        }
    }

    #[test]
    fn a_build_outside_a_release_adds_nothing() {
        let temp = tempdir().expect("tempdir is created");
        let mut env = HashMap::new();

        let exe = temp.path().join("target").join("debug").join("envoy");
        assert_eq!(apply_for(&mut env, &exe, None), None);
        assert!(env.is_empty());
    }

    #[test]
    fn the_bin_goes_ahead_of_the_envoy_a_stack_lists() {
        let bin = Path::new("/app/bin");
        let mut env = HashMap::from([(
            "PATH".to_string(),
            joined(&["/stack/envoy/v0.7.0/bin", "/tools"]),
        )]);

        add_to_env(&mut env, bin);

        assert_eq!(
            env["PATH"],
            joined(&["/app/bin", "/stack/envoy/v0.7.0/bin", "/tools"])
        );
    }

    #[test]
    fn a_bin_the_path_already_lists_is_moved_up_not_repeated() {
        let bin = Path::new("/app/bin");
        let mut env = HashMap::from([("PATH".to_string(), joined(&["/x", "/app/bin", "/y"]))]);

        add_to_env(&mut env, bin);

        assert_eq!(env["PATH"], joined(&["/app/bin", "/x", "/y"]));
    }

    #[test]
    fn with_no_path_the_bin_becomes_it() {
        let mut env = HashMap::new();

        add_to_env(&mut env, Path::new("/app/bin"));

        assert_eq!(env["PATH"], "/app/bin");
    }

    #[test]
    fn an_empty_path_leaves_no_blank_entry() {
        let mut env = HashMap::from([("PATH".to_string(), String::new())]);

        add_to_env(&mut env, Path::new("/app/bin"));

        assert_eq!(env["PATH"], "/app/bin");
    }

    #[test]
    fn however_the_variable_is_spelled_it_is_the_one_changed() {
        let mut env = HashMap::from([("Path".to_string(), joined(&["/x"]))]);

        add_to_env(&mut env, Path::new("/app/bin"));

        assert_eq!(env.len(), 1, "no second spelling is added: {env:?}");
        assert_eq!(env["Path"], joined(&["/app/bin", "/x"]));
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_same_directory_is_found_however_it_is_spelled() {
        let mut env = HashMap::from([("PATH".to_string(), "C:/X;C:/App/BIN/;C:/Y".to_string())]);

        add_to_env(&mut env, Path::new(r"c:\app\bin"));

        let entries: Vec<PathBuf> = std::env::split_paths(&OsString::from(&env["PATH"])).collect();
        assert_eq!(entries.len(), 3, "listed once: {entries:?}");
        assert_eq!(entries[0], Path::new(r"c:\app\bin"));
    }
}
