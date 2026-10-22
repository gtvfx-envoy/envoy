//! Reading the redirect file, with a saved copy to fall back on.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

use envoy_config::locate::redirect_candidates;
use envoy_config::redirect::Redirect;
use envoy_config::snapshot;

use crate::error::ShimError;

/// How long to wait for the share before falling back to a saved copy. A
/// network read that has gone wrong can otherwise hang for half a minute.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// Where a loaded redirect came from.
#[derive(Clone, Debug)]
pub enum Origin {
    /// Read from the share just now.
    Share(PathBuf),
    /// The share could not be used, so a saved copy was.
    Snapshot {
        /// The share path the copy was taken from.
        source: PathBuf,
        /// How long ago the copy was last confirmed against the share.
        age: Duration,
        /// Why the share could not be used.
        reason: String,
    },
}

/// A redirect file and where it came from.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub redirect: Redirect,
    pub origin: Origin,
}

/// Read the redirect file from the share, or from the copy saved in
/// `snapshot_dir` when the share cannot be used.
///
/// A copy is saved whenever the share is read, and only rewritten when the
/// file changed. The share is tried first even when a copy exists, so a change
/// to the file takes effect at the next launch. With no `snapshot_dir`, as when
/// caching is off, nothing is saved and there is nothing to fall back on.
pub fn load(
    env: &dyn Fn(&str) -> Option<String>,
    snapshot_dir: Option<&Path>,
    timeout: Duration,
) -> Result<Loaded, ShimError> {
    let candidates = redirect_candidates(env);
    if candidates.is_empty() {
        return Err(ShimError::Redirect(
            "no redirect file is configured. Set ENVOY_CONFIG_PROD to the directory that holds \
redirect.yaml, or ENVOY_REDIRECT_CONFIG to the file itself"
                .to_string(),
        ));
    }

    let mut problems = Vec::new();
    for path in &candidates {
        match read_with_timeout(path, timeout) {
            Ok(text) => match Redirect::parse(&text) {
                Ok(redirect) => {
                    // A copy is only a convenience, so failing to save one is not worth stopping for.
                    if let Some(dir) = snapshot_dir {
                        let _ = snapshot::save(dir, path, &text);
                    }
                    return Ok(Loaded {
                        redirect,
                        origin: Origin::Share(path.clone()),
                    });
                }
                Err(error) => problems.push(format!("{}: {error}", path.display())),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                problems.push(format!("{}: not found", path.display()));
            }
            Err(error) => problems.push(format!("{}: {error}", path.display())),
        }
    }

    for path in &candidates {
        let Some(saved) = snapshot_dir.and_then(|dir| snapshot::load(dir, path)) else {
            continue;
        };
        if let Ok(redirect) = Redirect::parse(&saved.text) {
            return Ok(Loaded {
                redirect,
                origin: Origin::Snapshot {
                    source: path.clone(),
                    age: SystemTime::now()
                        .duration_since(saved.confirmed)
                        .unwrap_or_default(),
                    reason: problems.join("; "),
                },
            });
        }
    }

    let fallback = if snapshot_dir.is_some() {
        ", and there is no saved copy to fall back on"
    } else {
        ""
    };
    Err(ShimError::Redirect(format!(
        "the redirect file could not be used{fallback}:\n  {}",
        problems.join("\n  ")
    )))
}

/// Read a file on a worker thread, giving up after `timeout`.
///
/// A thread that is still waiting on the share is simply left behind; it ends
/// with the process.
fn read_with_timeout(path: &Path, timeout: Duration) -> io::Result<String> {
    let (sender, receiver) = mpsc::channel();
    let path = path.to_path_buf();
    thread::spawn(move || {
        let _ = sender.send(fs::read_to_string(&path));
    });

    receiver.recv_timeout(timeout).unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("no answer within {} ms", timeout.as_millis()),
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tempfile::tempdir;

    use super::*;

    const REDIRECT: &str = "schema: 1\nsources: ['/s/{version}']\nversions: {default: v1}\n";

    fn env(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn explicit(path: &Path) -> impl Fn(&str) -> Option<String> {
        env(&[("ENVOY_REDIRECT_CONFIG", path.display().to_string())])
    }

    #[test]
    fn a_file_on_the_share_is_read_and_a_copy_is_kept() {
        let dir = tempdir().expect("tempdir is created");
        let file = dir.path().join("redirect.yaml");
        fs::write(&file, REDIRECT).expect("redirect is written");
        let snapshots = dir.path().join("snapshots");

        let loaded = load(&explicit(&file), Some(&snapshots), DEFAULT_TIMEOUT)
            .expect("the share is readable");

        assert!(matches!(loaded.origin, Origin::Share(ref path) if *path == file));
        assert!(snapshot::load(&snapshots, &file).is_some());
    }

    #[test]
    fn without_a_snapshot_directory_nothing_is_saved_or_fallen_back_on() {
        let dir = tempdir().expect("tempdir is created");
        let file = dir.path().join("redirect.yaml");
        fs::write(&file, REDIRECT).expect("redirect is written");
        load(&explicit(&file), None, DEFAULT_TIMEOUT).expect("the share is readable");
        fs::remove_file(&file).expect("the share goes away");

        let error = load(&explicit(&file), None, DEFAULT_TIMEOUT)
            .expect_err("there is no saved copy to use");

        assert!(!error.to_string().contains("saved copy"), "{error}");
        assert_eq!(
            fs::read_dir(dir.path()).expect("directory reads").count(),
            0
        );
    }

    #[test]
    fn when_the_share_goes_away_the_saved_copy_is_used_and_says_why() {
        let dir = tempdir().expect("tempdir is created");
        let file = dir.path().join("redirect.yaml");
        fs::write(&file, REDIRECT).expect("redirect is written");
        let snapshots = dir.path().join("snapshots");
        load(&explicit(&file), Some(&snapshots), DEFAULT_TIMEOUT).expect("first load");
        fs::remove_file(&file).expect("the share goes away");

        let loaded = load(&explicit(&file), Some(&snapshots), DEFAULT_TIMEOUT)
            .expect("the saved copy still serves");

        match loaded.origin {
            Origin::Snapshot { reason, .. } => assert!(reason.contains("not found"), "{reason}"),
            other => panic!("expected the saved copy, got {other:?}"),
        }
        assert_eq!(
            loaded.redirect.resolve("anyone").expect("resolves").version,
            "v1"
        );
    }

    #[test]
    fn an_invalid_file_on_the_share_falls_back_to_the_last_good_copy() {
        let dir = tempdir().expect("tempdir is created");
        let file = dir.path().join("redirect.yaml");
        fs::write(&file, REDIRECT).expect("redirect is written");
        let snapshots = dir.path().join("snapshots");
        load(&explicit(&file), Some(&snapshots), DEFAULT_TIMEOUT).expect("first load");
        fs::write(&file, "schema: 1\nversions: [broken\n").expect("someone breaks the file");

        let loaded = load(&explicit(&file), Some(&snapshots), DEFAULT_TIMEOUT)
            .expect("the last good copy still serves");

        assert!(matches!(loaded.origin, Origin::Snapshot { .. }));
    }

    #[test]
    fn with_no_file_and_no_copy_there_is_nothing_to_go_on() {
        let dir = tempdir().expect("tempdir is created");
        let missing = dir.path().join("redirect.yaml");

        let error = load(
            &explicit(&missing),
            Some(&dir.path().join("snapshots")),
            DEFAULT_TIMEOUT,
        )
        .expect_err("nothing can be loaded");

        assert!(error.to_string().contains("no saved copy"), "{error}");
        assert_eq!(error.exit_code(), crate::error::EXIT_CONFIG);
    }

    #[test]
    fn nothing_configured_says_what_to_set() {
        let error = load(&env(&[]), None, DEFAULT_TIMEOUT).expect_err("there is nowhere to look");

        assert!(error.to_string().contains("ENVOY_CONFIG_PROD"), "{error}");
    }

    #[test]
    fn a_missing_team_file_falls_through_to_the_studio_file() {
        let dir = tempdir().expect("tempdir is created");
        fs::write(dir.path().join("redirect.yaml"), REDIRECT).expect("redirect is written");
        let lookup = env(&[
            ("ENVOY_CONFIG_PROD", dir.path().display().to_string()),
            ("ENVOY_TEAM", "rigging".to_string()),
        ]);

        let loaded = load(
            &lookup,
            Some(&dir.path().join("snapshots")),
            DEFAULT_TIMEOUT,
        )
        .expect("the studio file serves");

        assert!(
            matches!(loaded.origin, Origin::Share(ref path) if path.ends_with("redirect.yaml"))
        );
        assert!(
            !matches!(&loaded.origin, Origin::Share(path) if path.to_string_lossy().contains("rigging"))
        );
    }
}
