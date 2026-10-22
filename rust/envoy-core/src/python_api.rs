//! The Python API that ships inside a release of envoy.
//!
//! A release bundle carries the `envoy` Python package unpacked in `py/`. When
//! envoy runs from one, the commands it launches can `import envoy` with
//! nothing in the stack providing it: the directory is named in
//! [`PATH_VAR`] and added to the end of `PYTHONPATH`, after anything the
//! stack's own bundles put there, so a stack that pins its own copy still wins.
//!
//! Set [`SWITCH_VAR`] to `off` to leave `PYTHONPATH` alone.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::bundle_store::parse_switch;

/// Names the directory of the Python API that ships with the running envoy.
pub const PATH_VAR: &str = "ENVOY_PYTHON_PATH";

/// `off`, `false`, `no` or `0` stops envoy adding its Python API to `PYTHONPATH`.
pub const SWITCH_VAR: &str = "ENVOY_PYTHON_API";

const PYTHONPATH: &str = "PYTHONPATH";

/// Return the `py` directory beside the `bin` directory that holds `exe`, when
/// `exe` is envoy's own executable and that directory holds the package.
///
/// Anything else, such as a development build or a Python interpreter running
/// the extension module, has no bundled API.
pub fn bundled_python_api(exe: &Path) -> Option<PathBuf> {
    let stem = exe.file_stem()?.to_str()?;
    if !stem.eq_ignore_ascii_case("envoy") {
        return None;
    }

    let api = exe.parent()?.parent()?.join("py");
    api.join("envoy").is_dir().then_some(api)
}

/// Add the Python API to the environment a command will run in.
///
/// `ENVOY_PYTHON_PATH` is always set to `api`. `PYTHONPATH` gets it appended
/// unless it is already listed, which keeps it from appearing twice when the
/// stack's own envoy bundle provides the same directory.
pub fn add_to_env(env: &mut HashMap<String, String>, api: &Path) {
    env.insert(PATH_VAR.to_string(), api.display().to_string());

    let mut entries: Vec<PathBuf> = env
        .get(PYTHONPATH)
        .map(|existing| std::env::split_paths(&OsString::from(existing)).collect())
        .unwrap_or_default();
    entries.retain(|entry| !entry.as_os_str().is_empty());

    if !entries.iter().any(|entry| same_path(entry, api)) {
        entries.push(api.to_path_buf());
    }
    if let Ok(joined) = std::env::join_paths(&entries) {
        env.insert(
            PYTHONPATH.to_string(),
            joined.to_string_lossy().into_owned(),
        );
    }
}

/// Add the running envoy's Python API to `env`, unless it is switched off or
/// envoy is not running from a release bundle.
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

    let api = bundled_python_api(exe)?;
    add_to_env(env, &api);
    Some(api)
}

fn same_path(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        let normalize = |path: &Path| path.to_string_lossy().replace('/', "\\").to_lowercase();
        normalize(left).trim_end_matches('\\') == normalize(right).trim_end_matches('\\')
    } else {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn release(root: &Path) -> PathBuf {
        fs::create_dir_all(root.join("bin")).expect("bin is created");
        fs::create_dir_all(root.join("py").join("envoy")).expect("package is created");
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
    fn a_release_bundle_has_its_python_api_beside_bin() {
        let temp = tempdir().expect("tempdir is created");
        let exe = release(temp.path());

        assert_eq!(bundled_python_api(&exe), Some(temp.path().join("py")));
    }

    #[test]
    fn only_envoys_own_release_layout_counts() {
        let temp = tempdir().expect("tempdir is created");
        let exe = release(temp.path());

        let python = temp.path().join("bin").join("python.exe");
        assert_eq!(bundled_python_api(&python), None, "not envoy's executable");

        fs::remove_dir_all(temp.path().join("py")).expect("py is removed");
        assert_eq!(
            bundled_python_api(&exe),
            None,
            "a development build has no py"
        );
        fs::create_dir_all(temp.path().join("py")).expect("an empty py is created");
        assert_eq!(bundled_python_api(&exe), None, "py without the package");
    }

    #[test]
    fn a_release_adds_its_api_unless_the_switch_is_off() {
        let temp = tempdir().expect("tempdir is created");
        let exe = release(temp.path());
        let api = temp.path().join("py");

        for switch in [None, Some("on"), Some("1"), Some("not a switch")] {
            let mut env = HashMap::new();
            assert_eq!(apply_for(&mut env, &exe, switch), Some(api.clone()));
            assert_eq!(env["ENVOY_PYTHON_PATH"], api.display().to_string());
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
    fn the_api_is_appended_after_whatever_the_stack_provides() {
        let api = Path::new("/app/py");
        let mut env = HashMap::from([("PYTHONPATH".to_string(), joined(&["/stack/py", "/other"]))]);

        add_to_env(&mut env, api);

        assert_eq!(env["ENVOY_PYTHON_PATH"], "/app/py");
        assert_eq!(
            env["PYTHONPATH"],
            joined(&["/stack/py", "/other", "/app/py"])
        );
    }

    #[test]
    fn a_path_the_stack_already_lists_is_not_added_again() {
        let api = Path::new("/app/py");
        let mut env = HashMap::from([("PYTHONPATH".to_string(), joined(&["/app/py", "/x"]))]);

        add_to_env(&mut env, api);

        assert_eq!(env["PYTHONPATH"], joined(&["/app/py", "/x"]));
    }

    #[test]
    fn with_no_pythonpath_the_api_becomes_it() {
        let mut env = HashMap::new();

        add_to_env(&mut env, Path::new("/app/py"));

        assert_eq!(env["PYTHONPATH"], "/app/py");
    }

    #[test]
    fn an_empty_pythonpath_leaves_no_blank_entry() {
        let mut env = HashMap::from([("PYTHONPATH".to_string(), String::new())]);

        add_to_env(&mut env, Path::new("/app/py"));

        assert_eq!(env["PYTHONPATH"], "/app/py");
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_same_directory_is_found_however_it_is_spelled() {
        let mut env = HashMap::from([("PYTHONPATH".to_string(), "C:/App/PY/".to_string())]);

        add_to_env(&mut env, Path::new(r"c:\app\py"));

        assert_eq!(env["PYTHONPATH"], "C:/App/PY/");
    }
}
