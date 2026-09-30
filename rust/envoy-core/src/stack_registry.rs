//! Named stack registry for `envoy`.
//!
//! Discovers versioned bundle-stack files stored in one or more *stack root*
//! directories. Each named stack lives in its own subdirectory and is
//! versioned by timestamp. When `latest.estack` is present, envoy prefers that
//! symlink; otherwise it falls back to the newest published version by
//! modified time.
//!
//! Directory layout under a stack root:
//!
//! ```text
//! <stack-root>/
//! └── studio/
//!     ├── 2026-06-21-101300/
//!     │   └── studio.estack
//!     ├── 2026-06-22-090000/
//!     │   └── studio.estack
//!     └── latest.estack -> 2026-06-22-090000/studio.estack
//! ```

use std::cmp::Ordering;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// Environment variable containing the stack root directories.
///
/// The value is semicolon-separated on Windows and colon-separated on Unix.
pub const STACK_ROOTS_VAR: &str = "ENVOY_STACK_ROOTS";

const LATEST_FILE: &str = "latest.estack";

/// A single named stack entry discovered from `ENVOY_STACK_ROOTS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedStackEntry {
    /// Named stack identifier.
    pub name: String,
    /// Version string derived from the published directory name.
    pub version: String,
    /// Resolved path to the latest published stack file.
    pub path: PathBuf,
    /// Stack root directory that provided this entry.
    pub stack_root: PathBuf,
}

fn stack_roots() -> Vec<PathBuf> {
    let roots_str = std::env::var(STACK_ROOTS_VAR).unwrap_or_default();

    roots_from_str(roots_str.trim())
}

fn roots_from_str(roots_str: &str) -> Vec<PathBuf> {
    if roots_str.is_empty() {
        return Vec::new();
    }

    let separator = if cfg!(windows) { ';' } else { ':' };

    roots_str
        .split(separator)
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(resolve_stack_root)
        .collect()
}

fn resolve_stack_root(root: &str) -> PathBuf {
    let path = Path::new(root);
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    fs::canonicalize(&absolute_path).unwrap_or_else(|_| lexical_normalize(&absolute_path))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    normalized
}

fn sorted_dir_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut paths: Vec<PathBuf> = read_dir
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();

    paths.sort_by_key(|path| path_name(path));
    paths
}

fn path_name(path: &Path) -> OsString {
    path.file_name()
        .map_or_else(OsString::new, OsStr::to_os_string)
}

fn published_stack(name_dir: &Path, name: &str, version_dir: &Path) -> Option<PathBuf> {
    if !version_dir.is_dir() || version_dir.parent() != Some(name_dir) {
        return None;
    }

    let stack_path = version_dir.join(format!("{name}.estack"));
    stack_path.is_file().then_some(stack_path)
}

fn latest_stack(name_dir: &Path, name: &str) -> Option<(String, PathBuf)> {
    let name_dir = fs::canonicalize(name_dir).ok()?;
    latest_stack_via_symlink(&name_dir, name)
        .or_else(|| latest_stack_via_modified_time(&name_dir, name))
}

fn latest_stack_via_symlink(name_dir: &Path, name: &str) -> Option<(String, PathBuf)> {
    let stack_path = fs::canonicalize(name_dir.join(LATEST_FILE)).ok()?;
    let version_dir = stack_path.parent()?;
    if version_dir.parent() != Some(name_dir)
        || stack_path.file_name() != Some(OsStr::new(&format!("{name}.estack")))
    {
        return None;
    }

    let version = version_dir.file_name()?.to_str()?.to_string();
    Some((version, stack_path))
}

fn latest_stack_via_modified_time(name_dir: &Path, name: &str) -> Option<(String, PathBuf)> {
    let mut latest: Option<(String, PathBuf, SystemTime)> = None;

    for version_dir in sorted_dir_paths(name_dir) {
        let Some(stack_path) = published_stack(name_dir, name, &version_dir) else {
            continue;
        };
        let Some(version) = version_dir.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let Ok(modified) = fs::metadata(&stack_path).and_then(|meta| meta.modified()) else {
            continue;
        };

        let is_newer = match &latest {
            None => true,
            Some((best_version, _, best_modified)) => match modified.partial_cmp(best_modified) {
                Some(Ordering::Greater) => true,
                Some(Ordering::Less) => false,
                Some(Ordering::Equal) | None => version > best_version.as_str(),
            },
        };

        if is_newer {
            latest = Some((version.to_string(), stack_path, modified));
        }
    }

    latest.map(|(version, stack_path, _)| (version, stack_path))
}

/// Return `true` if `value` looks like a named stack rather than a path.
///
/// A value is treated as a name when it contains no path separator characters
/// (`/`, `\`, `:`), does not start with a dot, and does not end in `.estack`.
/// Everything else is treated as a filesystem path.
///
/// # Examples
///
/// ```rust
/// # use envoy_core::stack_registry::is_stack_name;
/// assert!(is_stack_name("studio"));
/// assert!(is_stack_name("my-stack"));
/// assert!(!is_stack_name("/path/to/f.estack"));
/// assert!(!is_stack_name("R:/stacks/studio.estack"));
/// assert!(!is_stack_name("./relative.estack"));
/// ```
pub fn is_stack_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && !value.ends_with(".estack")
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains(':')
}

/// Resolve a named stack to the path of its latest version.
///
/// Searches each directory in `ENVOY_STACK_ROOTS` for a subdirectory named
/// `name`. Envoy prefers a valid `latest.estack` symlink when present and
/// otherwise falls back to the published version whose `.estack` file has the
/// newest modified time. Returns the first match.
pub fn resolve_named_stack(name: &str) -> Option<PathBuf> {
    for root in stack_roots() {
        let name_dir = root.join(name);
        if !name_dir.is_dir() {
            continue;
        }

        if let Some((_, stack_path)) = latest_stack(&name_dir, name) {
            return Some(stack_path);
        }
    }

    None
}

/// List all available named stacks across all `ENVOY_STACK_ROOTS` roots.
///
/// Scans each stack root for named subdirectories. For each name, envoy
/// prefers a valid `latest.estack` symlink when present and otherwise falls
/// back to the published version whose `.estack` file has the newest modified
/// time.
///
/// Deduplicates by name — the first root that defines a given name wins.
/// Returns entries sorted by name.
pub fn list_named_stacks() -> Vec<NamedStackEntry> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();

    for root in stack_roots() {
        if !root.is_dir() {
            continue;
        }

        for name_dir in sorted_dir_paths(&root) {
            if !name_dir.is_dir() {
                continue;
            }

            let Some(name_os) = name_dir.file_name() else {
                continue;
            };
            let name = name_os.to_string_lossy().into_owned();

            if seen.contains(&name) {
                continue;
            }

            let Some((version, stack_path)) = latest_stack(&name_dir, &name) else {
                continue;
            };

            entries.push(NamedStackEntry {
                name: name.clone(),
                version,
                path: stack_path,
                stack_root: root.clone(),
            });
            seen.insert(name);
        }
    }

    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

/// List all published versions of a named stack, newest first.
///
/// Returns an empty list if the name is not found in any root.
pub fn list_stack_versions(name: &str) -> Vec<(String, PathBuf)> {
    for root in stack_roots() {
        let name_dir = root.join(name);
        if !name_dir.is_dir() {
            continue;
        }

        let mut versions = Vec::new();
        for version_dir in sorted_dir_paths(&name_dir) {
            let Some(stack_path) = published_stack(&name_dir, name, &version_dir) else {
                continue;
            };
            let Some(version) = version_dir.file_name().and_then(OsStr::to_str) else {
                continue;
            };

            versions.push((version.to_string(), stack_path));
        }

        versions.sort_by(|left, right| right.0.cmp(&left.0));
        return versions;
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs::{self, File};
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use tempfile::tempdir;

    use super::{
        is_stack_name, list_named_stacks, list_stack_versions, resolve_named_stack, roots_from_str,
        NamedStackEntry, LATEST_FILE, STACK_ROOTS_VAR,
    };

    struct EnvVarGuard {
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(value: Option<&OsStr>) -> Self {
            let previous = std::env::var_os(STACK_ROOTS_VAR);

            match value {
                Some(value) => std::env::set_var(STACK_ROOTS_VAR, value),
                None => std::env::remove_var(STACK_ROOTS_VAR),
            }

            Self { previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(STACK_ROOTS_VAR, value),
                None => std::env::remove_var(STACK_ROOTS_VAR),
            }
        }
    }

    /// Locks the crate-wide `crate::env_test_lock::MUTEX` rather than a
    /// module-local mutex: both `stack_registry` and `discovery` tests
    /// mutate the same real `ENVOY_STACK_ROOTS` process environment variable,
    /// so a single shared lock is required to prevent cross-module test
    /// races under `cargo test`'s default parallel execution.
    fn with_stack_roots_env<T>(value: Option<&OsStr>, test_fn: impl FnOnce() -> T) -> T {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env_guard = EnvVarGuard::set(value);

        test_fn()
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }

    fn write_named_stack(root: &Path, name: &str, versions: &[(&str, &str)], latest: &str) -> bool {
        let name_dir = root.join(name);
        fs::create_dir_all(&name_dir).expect("failed to create named stack directory");

        for (version, contents) in versions {
            let version_dir = name_dir.join(version);
            fs::create_dir_all(&version_dir).expect("failed to create stack version directory");
            fs::write(version_dir.join(format!("{name}.estack")), contents)
                .expect("failed to write versioned stack");
        }

        create_file_symlink(
            &Path::new(latest).join(format!("{name}.estack")),
            &name_dir.join(LATEST_FILE),
        )
    }

    fn write_named_stack_versions(root: &Path, name: &str, versions: &[(&str, &str)]) {
        let name_dir = root.join(name);
        fs::create_dir_all(&name_dir).expect("failed to create named stack directory");

        for (version, contents) in versions {
            let version_dir = name_dir.join(version);
            fs::create_dir_all(&version_dir).expect("failed to create stack version directory");
            fs::write(version_dir.join(format!("{name}.estack")), contents)
                .expect("failed to write versioned stack");
        }
    }

    fn set_stack_modified_time(path: &Path, modified: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .expect("failed to open stack file for modified-time update")
            .set_modified(modified)
            .expect("failed to set stack file modified time");
    }

    fn join_roots(roots: &[&Path]) -> OsString {
        std::env::join_paths(roots).expect("failed to join stack roots")
    }

    #[test]
    fn roots_from_str_trims_entries_and_uses_absolute_paths() {
        let temp = tempdir().expect("failed to create temp dir");
        let existing_root = temp.path().join("existing");
        let missing_root = temp.path().join("missing");
        fs::create_dir_all(&existing_root).expect("failed to create existing root");

        // roots_from_str splits on ';' on Windows and ':' on Unix (matching
        // std::env::join_paths / the ENVOY_STACK_ROOTS_VAR convention).
        let separator = if cfg!(windows) { ';' } else { ':' };
        let roots = roots_from_str(&format!(
            "  {}  {separator}  {}  {separator}  ",
            existing_root.display(),
            missing_root.display()
        ));

        assert_eq!(roots.len(), 2);
        assert_eq!(
            roots[0],
            fs::canonicalize(&existing_root).expect("failed to canonicalize existing root")
        );
        assert_eq!(roots[1], missing_root);
    }

    #[test]
    fn is_stack_name_matches_python_examples() {
        assert!(is_stack_name("studio"));
        assert!(is_stack_name("my-stack"));
        assert!(!is_stack_name("studio.estack"));
        assert!(!is_stack_name("/path/to/f.estack"));
        assert!(!is_stack_name("R:/stacks/studio.estack"));
        assert!(!is_stack_name("./relative.estack"));
        assert!(!is_stack_name(""));
        assert!(!is_stack_name(".hidden"));
        assert!(!is_stack_name(r"relative\path.estack"));
    }

    #[test]
    fn resolve_named_stack_follows_latest_symlink() {
        let temp = tempdir().expect("failed to create temp dir");
        let stack_root = temp.path().join("stack-root");
        if !write_named_stack(
            &stack_root,
            "studio",
            &[("2026-06-22T09-00-00", "{}")],
            "2026-06-22T09-00-00",
        ) {
            return;
        }
        let roots = join_roots(&[stack_root.as_path()]);
        let expected_path = fs::canonicalize(
            stack_root
                .join("studio")
                .join("2026-06-22T09-00-00")
                .join("studio.estack"),
        )
        .expect("failed to canonicalize published path");

        with_stack_roots_env(Some(roots.as_os_str()), || {
            assert_eq!(resolve_named_stack("studio"), Some(expected_path));
        });
    }

    #[test]
    fn resolve_named_stack_falls_back_to_newest_modified_time_when_no_symlink() {
        let temp = tempdir().expect("failed to create temp dir");
        let stack_root = temp.path().join("stack-root");
        write_named_stack_versions(
            &stack_root,
            "studio",
            &[
                ("2026-01-01-000000", "{\"version\":\"older-name\"}"),
                ("2026-12-31-235959", "{\"version\":\"newer-name\"}"),
            ],
        );

        let newer_path = stack_root
            .join("studio")
            .join("2026-01-01-000000")
            .join("studio.estack");
        let older_path = stack_root
            .join("studio")
            .join("2026-12-31-235959")
            .join("studio.estack");
        let now = SystemTime::now();
        let one_hour_ago = now - Duration::from_secs(3600);

        set_stack_modified_time(&newer_path, now);
        set_stack_modified_time(&older_path, one_hour_ago);

        let roots = join_roots(&[stack_root.as_path()]);
        let expected_path =
            fs::canonicalize(&newer_path).expect("failed to canonicalize newest stack path");

        with_stack_roots_env(Some(roots.as_os_str()), || {
            assert_eq!(resolve_named_stack("studio"), Some(expected_path));
        });
    }

    #[test]
    fn resolve_named_stack_ties_break_by_version_name_when_no_symlink() {
        let temp = tempdir().expect("failed to create temp dir");
        let stack_root = temp.path().join("stack-root");
        write_named_stack_versions(
            &stack_root,
            "studio",
            &[
                ("2026-06-21-101300", "{\"version\":\"older\"}"),
                ("2026-06-22-090000", "{\"version\":\"newer\"}"),
            ],
        );

        let first_path = stack_root
            .join("studio")
            .join("2026-06-21-101300")
            .join("studio.estack");
        let second_path = stack_root
            .join("studio")
            .join("2026-06-22-090000")
            .join("studio.estack");
        let shared_modified = SystemTime::now();

        set_stack_modified_time(&first_path, shared_modified);
        set_stack_modified_time(&second_path, shared_modified);

        let roots = join_roots(&[stack_root.as_path()]);
        let expected_path =
            fs::canonicalize(&second_path).expect("failed to canonicalize tied stack path");

        with_stack_roots_env(Some(roots.as_os_str()), || {
            assert_eq!(resolve_named_stack("studio"), Some(expected_path));
        });
    }

    #[test]
    fn list_named_stacks_deduplicates_by_first_root_and_sorts_by_name() {
        let temp = tempdir().expect("failed to create temp dir");
        let root_a = temp.path().join("root-a");
        let root_b = temp.path().join("root-b");
        fs::create_dir_all(&root_a).expect("failed to create root-a");
        fs::create_dir_all(&root_b).expect("failed to create root-b");

        if !write_named_stack(
            &root_a,
            "beta",
            &[("2026-06-21T10-13-00", "{\"root\":\"a\"}")],
            "2026-06-21T10-13-00",
        ) {
            return;
        }
        if !write_named_stack(
            &root_b,
            "alpha",
            &[("2026-06-22T09-00-00", "{\"root\":\"b\"}")],
            "2026-06-22T09-00-00",
        ) {
            return;
        }
        if !write_named_stack(
            &root_b,
            "beta",
            &[("2026-06-23T11-00-00", "{\"root\":\"b\"}")],
            "2026-06-23T11-00-00",
        ) {
            return;
        }

        let roots = join_roots(&[root_a.as_path(), root_b.as_path()]);

        with_stack_roots_env(Some(roots.as_os_str()), || {
            let entries = list_named_stacks();
            let expected_root_a = fs::canonicalize(&root_a).expect("failed to canonicalize root-a");
            let expected_root_b = fs::canonicalize(&root_b).expect("failed to canonicalize root-b");

            assert_eq!(
                entries,
                vec![
                    NamedStackEntry {
                        name: String::from("alpha"),
                        version: String::from("2026-06-22T09-00-00"),
                        path: expected_root_b
                            .join("alpha")
                            .join("2026-06-22T09-00-00")
                            .join("alpha.estack"),
                        stack_root: expected_root_b,
                    },
                    NamedStackEntry {
                        name: String::from("beta"),
                        version: String::from("2026-06-21T10-13-00"),
                        path: expected_root_a
                            .join("beta")
                            .join("2026-06-21T10-13-00")
                            .join("beta.estack"),
                        stack_root: expected_root_a,
                    },
                ]
            );
        });
    }

    #[test]
    fn list_stack_versions_returns_newest_first_from_first_matching_root() {
        let temp = tempdir().expect("failed to create temp dir");
        let root_a = temp.path().join("root-a");
        let root_b = temp.path().join("root-b");
        if !write_named_stack(
            &root_a,
            "studio",
            &[("2026-06-21T10-13-00", "{}"), ("2026-06-22T09-00-00", "{}")],
            "2026-06-22T09-00-00",
        ) {
            return;
        }
        if !write_named_stack(
            &root_b,
            "studio",
            &[("2026-06-30T08-00-00", "{}")],
            "2026-06-30T08-00-00",
        ) {
            return;
        }
        fs::write(
            root_a.join("studio").join("2026-07-01T00-00-00.estack"),
            "{}",
        )
        .expect("failed to write ignored legacy stack");

        let roots = join_roots(&[root_a.as_path(), root_b.as_path()]);

        with_stack_roots_env(Some(roots.as_os_str()), || {
            let versions = list_stack_versions("studio");
            let expected_root_a = fs::canonicalize(&root_a).expect("failed to canonicalize root-a");

            assert_eq!(
                versions,
                vec![
                    (
                        String::from("2026-06-22T09-00-00"),
                        expected_root_a
                            .join("studio")
                            .join("2026-06-22T09-00-00")
                            .join("studio.estack"),
                    ),
                    (
                        String::from("2026-06-21T10-13-00"),
                        expected_root_a
                            .join("studio")
                            .join("2026-06-21T10-13-00")
                            .join("studio.estack"),
                    ),
                ]
            );
        });
    }
}
