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
    /// What is wrong with a file from the share that no saved copy could stand
    /// in for. It is used all the same, for the people it still works for.
    /// Empty for a good file and for a saved copy.
    pub problems: Vec<String>,
}

/// The first file that exists but cannot be used as it is.
struct Blocker {
    /// Its place among the candidates.
    index: usize,
    /// The file and what is wrong with it, when it could at least be parsed.
    flawed: Option<(Redirect, Vec<String>)>,
}

/// Read the redirect file from the share, or from the copy saved in
/// `snapshot_dir` when the share cannot be used.
///
/// The candidates are tried in order, and only a file that is not there lets
/// the next one have its turn. A file that is there but cannot be used (it
/// does not parse, is written for a newer launcher, cannot be read, or would
/// leave someone with no version) stops the search: a team whose own file is
/// broken must not quietly run the studio's version. Its saved copy is used
/// instead, and failing that the file itself, for those it still works for.
///
/// A copy is saved whenever a good file is read, and only rewritten when the
/// file changed, so a bad edit never replaces the last good copy. The share is
/// tried first even when a copy exists, so a change to the file takes effect at
/// the next launch. With no `snapshot_dir`, as when caching is off, nothing is
/// saved and there is nothing to fall back on.
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
    let mut blocker = None;
    for (index, path) in candidates.iter().enumerate() {
        match read_with_timeout(path, timeout) {
            Ok(text) => match Redirect::parse(&text) {
                Ok(redirect) => {
                    let blocking = redirect.blocking_problems();
                    if blocking.is_empty() {
                        // A copy is only a convenience, so failing to save one is not worth stopping for.
                        if let Some(dir) = snapshot_dir {
                            let _ = snapshot::save(dir, path, &text);
                        }
                        return Ok(Loaded {
                            redirect,
                            origin: Origin::Share(path.clone()),
                            problems: Vec::new(),
                        });
                    }
                    let messages: Vec<String> = blocking
                        .into_iter()
                        .map(|problem| problem.message)
                        .collect();
                    problems.push(format!("{}: {}", path.display(), messages.join("; ")));
                    blocker = Some(Blocker {
                        index,
                        flawed: Some((redirect, messages)),
                    });
                    break;
                }
                Err(error) => {
                    problems.push(format!("{}: {error}", path.display()));
                    blocker = Some(Blocker {
                        index,
                        flawed: None,
                    });
                    break;
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                problems.push(format!("{}: not found", path.display()));
            }
            Err(error) => {
                problems.push(format!("{}: {error}", path.display()));
                blocker = Some(Blocker {
                    index,
                    flawed: None,
                });
                break;
            }
        }
    }

    // With nothing in the way, every candidate has a saved copy to offer, best
    // first. With a file in the way, only its own copy will do.
    let order: Vec<usize> = match &blocker {
        Some(blocker) => vec![blocker.index],
        None => (0..candidates.len()).collect(),
    };
    for index in order {
        let path = &candidates[index];
        let Some(saved) = snapshot_dir.and_then(|dir| snapshot::load(dir, path)) else {
            continue;
        };
        let Ok(redirect) = Redirect::parse(&saved.text) else {
            continue;
        };
        if !redirect.blocking_problems().is_empty() {
            continue;
        }
        return Ok(Loaded {
            redirect,
            origin: Origin::Snapshot {
                source: path.clone(),
                age: SystemTime::now()
                    .duration_since(saved.confirmed)
                    .unwrap_or_default(),
                reason: problems.join("; "),
            },
            problems: Vec::new(),
        });
    }

    if let Some(Blocker {
        index,
        flawed: Some((redirect, messages)),
    }) = blocker
    {
        return Ok(Loaded {
            redirect,
            origin: Origin::Share(candidates[index].clone()),
            problems: messages,
        });
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
pub(crate) fn read_with_timeout(path: &Path, timeout: Duration) -> io::Result<String> {
    let path = path.to_path_buf();
    within(timeout, move || fs::read_to_string(&path))
}

/// Run `work` on a worker thread, giving up after `timeout`.
///
/// A thread that is still waiting on the share is simply left behind; it ends
/// with the process.
pub(crate) fn within<T: Send + 'static>(
    timeout: Duration,
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(work());
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

    const TEAM: &str = "schema: 1\nsources: ['/s/{version}']\nversions: {default: v2}\n";

    /// A share holding the studio file and a team file, and the lookup that selects the team.
    fn team_share() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        impl Fn(&str) -> Option<String>,
    ) {
        let dir = tempdir().expect("tempdir is created");
        let studio = dir.path().join("redirect.yaml");
        let team = dir.path().join("rigging").join("redirect.yaml");
        fs::create_dir_all(team.parent().expect("a parent")).expect("team folder is created");
        fs::write(&studio, REDIRECT).expect("studio file is written");
        fs::write(&team, TEAM).expect("team file is written");
        let lookup = env(&[
            ("ENVOY_CONFIG_PROD", dir.path().display().to_string()),
            ("ENVOY_TEAM", "rigging".to_string()),
        ]);
        (dir, studio, team, lookup)
    }

    fn version_of(loaded: &Loaded) -> String {
        loaded.redirect.resolve("anyone").expect("resolves").version
    }

    #[test]
    fn a_broken_team_file_is_never_replaced_by_the_studio_file() {
        let (dir, _, team, lookup) = team_share();
        let snapshots = dir.path().join("snapshots");
        let first = load(&lookup, Some(&snapshots), DEFAULT_TIMEOUT).expect("the team file serves");
        assert_eq!(version_of(&first), "v2");

        fs::write(&team, "schema: 1\nversions: {default: [v2\n").expect("someone breaks it");
        let second = load(&lookup, Some(&snapshots), DEFAULT_TIMEOUT)
            .expect("the team's last good copy serves");

        match &second.origin {
            Origin::Snapshot { source, reason, .. } => {
                assert_eq!(source, &team);
                assert!(reason.contains("rigging"), "{reason}");
            }
            other => panic!("expected the team's saved copy, got {other:?}"),
        }
        assert_eq!(version_of(&second), "v2", "not the studio's v1");

        // A machine that never read the team file has nothing of the team's to use.
        let error = load(
            &lookup,
            Some(&dir.path().join("elsewhere")),
            DEFAULT_TIMEOUT,
        )
        .expect_err("the studio file is not a stand-in");
        assert!(error.to_string().contains("rigging"), "{error}");
        assert_eq!(error.exit_code(), crate::error::EXIT_CONFIG);
    }

    #[test]
    fn a_team_file_that_cannot_be_read_does_not_fall_through_either() {
        let (dir, _, team, lookup) = team_share();
        let snapshots = dir.path().join("snapshots");
        load(&lookup, Some(&snapshots), DEFAULT_TIMEOUT).expect("the team file serves");

        // A directory where the file was: it exists, but cannot be read as a file.
        fs::remove_file(&team).expect("the file goes");
        fs::create_dir(&team).expect("a folder takes its place");
        let loaded =
            load(&lookup, Some(&snapshots), DEFAULT_TIMEOUT).expect("the saved copy serves");

        assert!(matches!(loaded.origin, Origin::Snapshot { .. }));
        assert_eq!(version_of(&loaded), "v2");
    }

    #[test]
    fn a_studio_file_in_the_way_is_not_passed_over_for_a_stale_team_copy() {
        let (dir, studio, team, lookup) = team_share();
        let snapshots = dir.path().join("snapshots");
        // Each file is read once, so each has a saved copy.
        let without_team = env(&[("ENVOY_CONFIG_PROD", dir.path().display().to_string())]);
        load(&without_team, Some(&snapshots), DEFAULT_TIMEOUT).expect("the studio file serves");
        load(&lookup, Some(&snapshots), DEFAULT_TIMEOUT).expect("the team file serves");

        // The team's file is withdrawn and the studio file is broken.
        fs::remove_file(&team).expect("the team file goes");
        fs::write(&studio, "schema: 1\nversions: [broken\n").expect("the studio file breaks");
        let loaded = load(&lookup, Some(&snapshots), DEFAULT_TIMEOUT)
            .expect("the studio's last good copy serves");

        assert!(matches!(&loaded.origin, Origin::Snapshot { source, .. } if *source == studio));
        assert_eq!(version_of(&loaded), "v1");
    }

    #[test]
    fn a_bad_edit_never_replaces_the_last_good_copy() {
        let dir = tempdir().expect("tempdir is created");
        let file = dir.path().join("redirect.yaml");
        fs::write(&file, REDIRECT).expect("redirect is written");
        let snapshots = dir.path().join("snapshots");
        load(&explicit(&file), Some(&snapshots), DEFAULT_TIMEOUT).expect("first load");

        // It parses, but would give everyone a version that is not a safe name.
        let unusable = "schema: 1\nsources: ['/s/{version}']\nversions: {default: 'v1/x'}\n";
        fs::write(&file, unusable).expect("someone edits it");
        let loaded = load(&explicit(&file), Some(&snapshots), DEFAULT_TIMEOUT)
            .expect("the last good copy serves");

        match &loaded.origin {
            Origin::Snapshot { reason, .. } => {
                assert!(reason.contains("not a safe version name"), "{reason}");
            }
            other => panic!("expected the saved copy, got {other:?}"),
        }
        assert_eq!(version_of(&loaded), "v1");
        let kept = snapshot::load(&snapshots, &file).expect("a copy is still saved");
        assert_eq!(kept.text, REDIRECT, "the bad text was not saved over it");
    }

    #[test]
    fn with_no_good_copy_a_file_with_problems_still_serves_those_it_works_for() {
        let dir = tempdir().expect("tempdir is created");
        let file = dir.path().join("redirect.yaml");
        fs::write(
            &file,
            "schema: 1\nsources: ['/s/{version}']\nversions: {default: v1}\nusers: {alice: typo}\n",
        )
        .expect("redirect is written");
        let snapshots = dir.path().join("snapshots");

        let loaded = load(&explicit(&file), Some(&snapshots), DEFAULT_TIMEOUT)
            .expect("the file is used anyway");

        assert!(matches!(loaded.origin, Origin::Share(_)));
        assert_eq!(loaded.problems.len(), 1, "{:?}", loaded.problems);
        assert!(
            loaded.problems[0].contains("group 'typo'"),
            "{:?}",
            loaded.problems
        );
        assert_eq!(
            loaded.redirect.resolve("bob").expect("bob is fine").version,
            "v1"
        );
        assert!(loaded.redirect.resolve("alice").is_err(), "alice is not");
        assert!(
            snapshot::load(&snapshots, &file).is_none(),
            "a file with problems is not kept as the last good copy"
        );
    }

    #[test]
    fn a_file_that_does_not_parse_and_has_no_copy_is_an_error() {
        let dir = tempdir().expect("tempdir is created");
        let file = dir.path().join("redirect.yaml");
        fs::write(&file, "schema: 2\nsurprise: [this, is, new]\n").expect("redirect is written");

        let error = load(
            &explicit(&file),
            Some(&dir.path().join("snapshots")),
            DEFAULT_TIMEOUT,
        )
        .expect_err("nothing to go on");

        assert!(error.to_string().contains("schema 2"), "{error}");
        assert!(error.to_string().contains("no saved copy"), "{error}");
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
