//! Keeping the last good copy of a file from the share.
//!
//! A snapshot lets envoy start when the share is unreachable. It is the file's
//! text, untouched, so anything that can parse the original can parse it. The
//! file's modified time says when it was last confirmed against the share:
//! reading the share again leaves the text alone when it has not changed, but
//! refreshes the time.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{ConfigError, Result};

/// A saved copy of a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    /// The file's text as it was read from the share.
    pub text: String,
    /// When it was last confirmed against the share.
    pub confirmed: SystemTime,
}

/// The name a snapshot of `source` is kept under in the snapshot directory.
///
/// Different sources, such as two teams' files, get different names, so one
/// never overwrites another.
pub fn name_for(source: &Path) -> String {
    format!(
        "redirect-{:08x}.yaml",
        fingerprint(&source.to_string_lossy())
    )
}

/// FNV-1a: small, stable across platforms and releases, and enough to tell a
/// handful of paths apart.
fn fingerprint(text: &str) -> u32 {
    text.bytes().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// Return the snapshot of `source` kept in `dir`, if there is one.
pub fn load(dir: &Path, source: &Path) -> Option<Snapshot> {
    let path = dir.join(name_for(source));
    let text = fs::read_to_string(&path).ok()?;
    let confirmed = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()?;
    Some(Snapshot { text, confirmed })
}

/// Record `text`, just read from `source`, in `dir`.
///
/// Returns `true` when the snapshot was written and `false` when it already
/// held exactly this text, in which case only its confirmation time is
/// refreshed. A write goes to a temporary file first and is renamed into
/// place, so a reader never sees half a file.
pub fn save(dir: &Path, source: &Path, text: &str) -> Result<bool> {
    let io_error = |path: &Path| {
        let path = path.to_path_buf();
        move |source| ConfigError::Io { path, source }
    };

    fs::create_dir_all(dir).map_err(io_error(dir))?;
    let path = dir.join(name_for(source));

    if fs::read_to_string(&path).is_ok_and(|existing| existing == text) {
        confirm(&path);
        return Ok(false);
    }

    let temporary = temporary_path(&path);
    fs::write(&temporary, text).map_err(io_error(&temporary))?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error(&path)(error));
    }
    Ok(true)
}

/// Refresh a snapshot's confirmation time. Best effort: a read-only
/// snapshot still serves.
fn confirm(path: &Path) {
    let opened: io::Result<File> = OpenOptions::new().write(true).open(path);
    if let Ok(file) = opened {
        let _ = file.set_modified(SystemTime::now());
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".tmp{}", std::process::id()));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn a_saved_snapshot_is_found_again() {
        let dir = tempdir().expect("tempdir is created");
        let source = Path::new("//server/config/redirect.yaml");

        assert!(load(dir.path(), source).is_none());
        assert!(save(dir.path(), source, "schema: 1\n").expect("first save"));

        let snapshot = load(dir.path(), source).expect("the snapshot is found");
        assert_eq!(snapshot.text, "schema: 1\n");
    }

    #[test]
    fn saving_the_same_text_again_only_refreshes_the_confirmation_time() {
        let dir = tempdir().expect("tempdir is created");
        let source = Path::new("/share/redirect.yaml");
        save(dir.path(), source, "same").expect("first save");
        let path = dir.path().join(name_for(source));
        File::options()
            .write(true)
            .open(&path)
            .expect("snapshot opens")
            .set_modified(SystemTime::now() - Duration::from_secs(3 * 86_400))
            .expect("snapshot is backdated");

        let written = save(dir.path(), source, "same").expect("second save");

        assert!(!written, "nothing changed, so nothing was rewritten");
        let age = SystemTime::now()
            .duration_since(load(dir.path(), source).expect("found").confirmed)
            .expect("the time is in the past");
        assert!(
            age < Duration::from_secs(60),
            "confirmed just now, not {age:?} ago"
        );
    }

    #[test]
    fn changed_text_replaces_the_snapshot_whole() {
        let dir = tempdir().expect("tempdir is created");
        let source = Path::new("/share/redirect.yaml");
        save(dir.path(), source, "old").expect("first save");

        assert!(save(dir.path(), source, "new text").expect("second save"));

        assert_eq!(load(dir.path(), source).expect("found").text, "new text");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("directory reads")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "no temporary file is left: {leftovers:?}"
        );
    }

    #[test]
    fn different_sources_never_share_a_snapshot() {
        let dir = tempdir().expect("tempdir is created");
        let studio = Path::new("//server/config/redirect.yaml");
        let team = Path::new("//server/config/rigging/redirect.yaml");

        save(dir.path(), studio, "studio").expect("save");
        save(dir.path(), team, "team").expect("save");

        assert_ne!(name_for(studio), name_for(team));
        assert_eq!(load(dir.path(), studio).expect("found").text, "studio");
        assert_eq!(load(dir.path(), team).expect("found").text, "team");
    }
}
