//! Making a version of the envoy app runnable.

use std::path::{Path, PathBuf};

use envoy_config::expand::expand_source;
use envoy_store::settings::Location;
use envoy_store::{EntryKey, Lease, Observer, Store};

use crate::error::ShimError;

/// The app is cached as a store entry under this namespace and name.
const APP_NAMESPACE: &str = "app";
const APP_NAME: &str = "envoy";

/// Where the app's executable is, relative to the app's directory.
pub fn exe_relative() -> PathBuf {
    Path::new("bin").join(if cfg!(windows) { "envoy.exe" } else { "envoy" })
}

/// The store entry that holds `version` of the app.
pub fn key_for(version: &str) -> Result<EntryKey, ShimError> {
    EntryKey::new(APP_NAMESPACE, APP_NAME, version)
        .map_err(|error| ShimError::Internal(format!("envoy {version} cannot be cached: {error}")))
}

/// Where a runnable app was found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Found {
    /// A local copy in the store.
    Cache(PathBuf),
    /// Run in place from its source.
    Source(PathBuf),
}

/// A version of the app that is ready to run.
#[derive(Debug)]
pub struct App {
    pub exe: PathBuf,
    pub found: Found,
    /// Keeps a cached copy from being pruned while it runs.
    _lease: Option<Lease>,
}

impl App {
    fn cached(path: PathBuf, lease: Lease) -> Self {
        Self {
            exe: path.join(exe_relative()),
            found: Found::Cache(path),
            _lease: Some(lease),
        }
    }

    fn from_source(path: PathBuf) -> Self {
        Self {
            exe: path.join(exe_relative()),
            found: Found::Source(path),
            _lease: None,
        }
    }
}

/// Find the first source that holds `version` of the app.
///
/// Each source is a template; the first whose directory holds the executable
/// wins. On failure the list says what was tried and why each was no good.
pub fn find_source(
    version: &str,
    sources: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf, Vec<String>> {
    let mut tried = Vec::new();
    for template in sources {
        match expand_source(template, version, env) {
            Ok(directory) => {
                if directory.join(exe_relative()).is_file() {
                    return Ok(directory);
                }
                tried.push(format!(
                    "{}: there is no {} there",
                    directory.display(),
                    exe_relative().display()
                ));
            }
            Err(error) => tried.push(error.to_string()),
        }
    }
    Err(tried)
}

/// Make `version` of the app ready to run.
///
/// A cached copy is used without touching the share at all. Otherwise the
/// first source that has the app is copied into the store and run from there.
/// With caching off, or when the store cannot take it, the app runs in place
/// from its source, so a caching problem never stops envoy from starting.
pub fn ensure(
    version: &str,
    sources: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    location: &Location,
    observer: &dyn Observer,
) -> Result<App, ShimError> {
    let key = key_for(version)?;
    let store = open_store(location);

    if let Some(store) = &store {
        if store.lookup(&key).is_some() {
            // Nothing is read from the source when the entry is already there.
            if let Ok(ensured) = store.ensure(&key, Path::new(""), observer) {
                return Ok(App::cached(ensured.path, ensured.lease));
            }
        }
    }

    let source =
        find_source(version, sources, env).map_err(|tried| unavailable(version, &tried))?;
    let Some(store) = store else {
        return Ok(App::from_source(source));
    };

    match store.ensure(&key, &source, observer) {
        Ok(ensured) => Ok(App::cached(ensured.path, ensured.lease)),
        Err(error) => {
            eprintln!(
                "envoy: could not cache envoy {version} ({error}); running it from {}",
                source.display()
            );
            Ok(App::from_source(source))
        }
    }
}

fn open_store(location: &Location) -> Option<Store> {
    if !location.enabled {
        return None;
    }
    match Store::open(&location.root) {
        Ok(store) => Some(store),
        Err(error) => {
            eprintln!("envoy: caching is unavailable, so envoy runs from its source: {error}");
            None
        }
    }
}

fn unavailable(version: &str, tried: &[String]) -> ShimError {
    let looked = if tried.is_empty() {
        "the redirect file lists no sources".to_string()
    } else {
        format!("looked in:\n  {}", tried.join("\n  "))
    };
    ShimError::Unavailable(format!(
        "envoy {version} is not cached and could not be found ({looked}).\n\
Check that the share is reachable, or that {version} has been published."
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use envoy_store::Quiet;
    use tempfile::tempdir;

    use super::*;

    fn publish(share: &Path, version: &str) -> PathBuf {
        let directory = share.join(version);
        let exe = directory.join(exe_relative());
        fs::create_dir_all(exe.parent().expect("exe has a parent")).expect("bin is created");
        fs::write(&exe, "pretend executable").expect("exe is written");
        fs::write(directory.join("note.txt"), version).expect("note is written");
        directory
    }

    fn sources(share: &Path) -> Vec<String> {
        vec![format!("{}/{{version}}", share.display())]
    }

    fn location(root: &Path, enabled: bool) -> Location {
        Location {
            enabled,
            enabled_by: "the test",
            root: root.to_path_buf(),
        }
    }

    fn no_env(_name: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_version_is_copied_into_the_store_and_then_needs_no_share() {
        let temp = tempdir().expect("tempdir is created");
        let share = temp.path().join("share");
        publish(&share, "v1");
        let store = location(&temp.path().join("store"), true);

        let first = ensure("v1", &sources(&share), &no_env, &store, &Quiet).expect("fills");
        assert!(matches!(first.found, Found::Cache(ref path) if path.starts_with(&store.root)));
        assert!(first.exe.is_file());
        drop(first);

        fs::remove_dir_all(&share).expect("the share goes away");
        let second = ensure("v1", &sources(&share), &no_env, &store, &Quiet)
            .expect("a cached version needs no share");
        assert!(matches!(second.found, Found::Cache(_)));
        assert!(second.exe.is_file());
    }

    #[test]
    fn a_version_that_is_nowhere_says_where_it_looked() {
        let temp = tempdir().expect("tempdir is created");
        let share = temp.path().join("share");
        publish(&share, "v1");

        let error = ensure(
            "v9",
            &sources(&share),
            &no_env,
            &location(&temp.path().join("store"), true),
            &Quiet,
        )
        .expect_err("v9 does not exist");

        let message = error.to_string();
        assert!(message.contains("envoy v9 is not cached"), "{message}");
        assert!(message.contains("share"), "{message}");
        assert_eq!(error.exit_code(), crate::error::EXIT_UNAVAILABLE);
    }

    #[test]
    fn the_first_source_that_holds_the_app_wins() {
        let temp = tempdir().expect("tempdir is created");
        let old_layout = temp.path().join("old");
        let new_layout = temp.path().join("new");
        publish(&new_layout, "v1");
        fs::create_dir_all(old_layout.join("v1")).expect("an empty directory is not a version");
        let both = vec![
            format!("{}/{{version}}", old_layout.display()),
            "${UNSET_VARIABLE}/{version}".to_string(),
            format!("{}/{{version}}", new_layout.display()),
        ];

        let found = find_source("v1", &both, &no_env).expect("the third source holds it");

        assert_eq!(found, new_layout.join("v1"));
        let tried = find_source("v2", &both, &no_env).expect_err("nothing holds v2");
        assert_eq!(tried.len(), 3);
        assert!(tried
            .iter()
            .any(|reason| reason.contains("UNSET_VARIABLE is not set")));
    }

    #[test]
    fn with_caching_off_the_app_runs_from_its_source() {
        let temp = tempdir().expect("tempdir is created");
        let share = temp.path().join("share");
        publish(&share, "v1");
        let store_root = temp.path().join("store");

        let app = ensure(
            "v1",
            &sources(&share),
            &no_env,
            &location(&store_root, false),
            &Quiet,
        )
        .expect("runs in place");

        assert_eq!(app.found, Found::Source(share.join("v1")));
        assert!(
            !store_root.exists(),
            "an off switch must not create the store"
        );
    }

    #[test]
    fn a_store_that_cannot_be_used_falls_back_to_the_source() {
        let temp = tempdir().expect("tempdir is created");
        let share = temp.path().join("share");
        publish(&share, "v1");
        let blocker = temp.path().join("a-file");
        fs::write(&blocker, "not a directory").expect("blocker is written");

        let app = ensure(
            "v1",
            &sources(&share),
            &no_env,
            &location(&blocker.join("store"), true),
            &Quiet,
        )
        .expect("runs in place");

        assert!(matches!(app.found, Found::Source(_)));
    }

    #[test]
    fn a_version_that_cannot_be_a_directory_name_is_refused() {
        let error = key_for("../escape").expect_err("unsafe versions are refused");

        assert_eq!(error.exit_code(), crate::error::EXIT_SOFTWARE);
    }
}
