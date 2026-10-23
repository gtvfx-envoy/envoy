//! The last good resolution of a stack, kept so that a stack that has run on
//! this machine can start again when the share is unreachable.
//!
//! A snapshot records what a selector (a stack name such as `studio`, or the
//! path of an `.estack` file) came to: the stack's name and version, and its
//! bundles in order, each with its identity and, for a published bundle, its
//! version. That is everything needed to find those bundles in the local
//! store, which holds complete copies of what has been run, so nothing is read
//! from the share to load them.
//!
//! Snapshots live under `<cache root>/snapshots/stacks/`, one small JSON file
//! per selector. A copy is written when a stack is first resolved or changes,
//! and its modified time says when the stack was last resolved from its source.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::discovery::lexical_normalize;
use crate::stack_registry::is_stack_name;

/// The layout of a snapshot file. A file with a newer layout is ignored.
const SCHEMA: u32 = 1;

/// Where, under the cache root, snapshots are kept.
const FOLDER: [&str; 2] = ["snapshots", "stacks"];

/// A confirmation younger than this is not refreshed, so a warm run does not
/// write a file every time.
const CONFIRM_EVERY: Duration = Duration::from_secs(60 * 60);

/// What a user selected: a stack published under a name, or an `.estack` file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Selector {
    /// A name published in `ENVOY_STACK_ROOTS`.
    Name(String),
    /// The absolute path of an `.estack` file.
    Path(PathBuf),
}

impl Selector {
    /// Read what a person typed: a name, or a path. A path is made absolute
    /// and tidied but not resolved against the disk, so that it reads the same
    /// whether or not the file can be reached.
    pub fn parse(raw: &str) -> Self {
        if is_stack_name(raw) {
            Self::Name(raw.to_string())
        } else {
            let path = Path::new(raw);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                env::current_dir()
                    .map(|cwd| cwd.join(path))
                    .unwrap_or_else(|_| path.to_path_buf())
            };
            Self::Path(lexical_normalize(&absolute))
        }
    }

    /// A text that tells one selector from another.
    pub fn key(&self) -> String {
        match self {
            Self::Name(name) => format!("name:{name}"),
            Self::Path(path) => {
                let text = path.to_string_lossy();
                if cfg!(windows) {
                    format!("path:{}", text.to_lowercase())
                } else {
                    format!("path:{text}")
                }
            }
        }
    }

    fn file_name(&self) -> String {
        let readable: String = match self {
            Self::Name(name) => name
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .take(40)
                .collect(),
            Self::Path(_) => "path".to_string(),
        };
        format!("{readable}-{:016x}.json", fingerprint(&self.key()))
    }
}

/// FNV-1a: small, stable across platforms and releases, and enough to tell a
/// handful of selectors apart. The key stored inside the file catches a clash.
fn fingerprint(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A stack file's size and modified time, which tell whether it has changed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileStamp {
    /// Size in bytes.
    pub len: u64,
    /// Seconds since the epoch.
    pub modified: Option<u64>,
}

/// The stamp of the file at `path`, or `None` when it cannot be read.
pub fn stamp_of(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs());
    Some(FileStamp {
        len: metadata.len(),
        modified,
    })
}

/// One bundle of a stack, as it was resolved.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SnapshotBundle {
    /// Where the stack found it.
    pub path: PathBuf,
    /// Its namespace.
    pub namespace: String,
    /// Its name.
    pub name: String,
    /// The version its marker states; `None` for a checkout, which is never cached.
    pub version: Option<String>,
    /// What the stack attached to the entry.
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}

impl SnapshotBundle {
    /// `namespace:name@version`, or `namespace:name` for a checkout.
    pub fn label(&self) -> String {
        match &self.version {
            Some(version) => format!("{}:{}@{version}", self.namespace, self.name),
            None => format!("{}:{}", self.namespace, self.name),
        }
    }
}

/// How a stack resolved, as kept in a snapshot file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Snapshot {
    /// The layout of this file.
    pub schema: u32,
    /// [`Selector::key`], which catches two selectors that share a file name.
    pub selector: String,
    /// The stack file it was read from.
    pub path: PathBuf,
    /// That file's size and modified time when it was read.
    pub stamp: Option<FileStamp>,
    /// The stack's name.
    pub name: String,
    /// The stack's namespace.
    pub namespace: String,
    /// The publication version of a named stack.
    pub registry_version: Option<String>,
    /// The stack's informational pinned version.
    pub pinned_version: Option<String>,
    /// The stack's own metadata.
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
    /// The stack's bundles, in order.
    pub bundles: Vec<SnapshotBundle>,
}

impl Snapshot {
    /// Start a snapshot of the stack read from `path` as it is now.
    pub fn new(selector: &Selector, path: PathBuf, stamp: Option<FileStamp>) -> Self {
        Self {
            schema: SCHEMA,
            selector: selector.key(),
            path,
            stamp,
            name: String::new(),
            namespace: String::new(),
            registry_version: None,
            pinned_version: None,
            metadata: HashMap::new(),
            bundles: Vec::new(),
        }
    }

    /// Whether this snapshot was taken from `path` exactly as it is now.
    pub fn is_of(&self, path: &Path, stamp: Option<FileStamp>) -> bool {
        stamp.is_some() && self.path == path && self.stamp == stamp
    }
}

/// What a stack that was rebuilt from a snapshot says about itself.
#[derive(Clone, Debug)]
pub struct SnapshotInfo {
    /// When the stack was last resolved from its source.
    pub confirmed: SystemTime,
    /// Why the source was not used.
    pub reason: String,
    /// Bundles the stack lists that are not on this machine, as `namespace:name@version`.
    pub unavailable: Vec<String>,
}

impl SnapshotInfo {
    /// How long ago the stack was last resolved from its source, for people,
    /// such as `3 hours`.
    pub fn ago(&self) -> String {
        ago(self.confirmed)
    }

    /// What to tell a person who has just started envoy from a snapshot.
    pub fn describe(&self, stack: &str, registry_version: Option<&str>) -> Vec<String> {
        let version = registry_version
            .map(|version| format!(" (published {version})"))
            .unwrap_or_default();
        let mut lines = vec![format!(
            "envoy: {}; using stack '{stack}'{version} as it was {} ago",
            self.reason,
            self.ago()
        )];
        if !self.unavailable.is_empty() {
            lines.push(format!(
                "envoy: not cached on this machine, so unavailable: {}",
                self.unavailable.join(", ")
            ));
        }
        lines
    }
}

/// How long ago `since` was, for people, such as `3 hours`.
pub fn ago(since: SystemTime) -> String {
    let seconds = SystemTime::now()
        .duration_since(since)
        .unwrap_or_default()
        .as_secs();
    let (number, unit) = match seconds {
        0..=59 => return "a moment".to_string(),
        60..=3_599 => (seconds / 60, "minute"),
        3_600..=86_399 => (seconds / 3_600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    format!("{number} {unit}{}", if number == 1 { "" } else { "s" })
}

/// The folder under `root` that holds stack snapshots.
pub fn folder(root: &Path) -> PathBuf {
    FOLDER
        .iter()
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

fn path_for(root: &Path, selector: &Selector) -> PathBuf {
    folder(root).join(selector.file_name())
}

/// The snapshot kept for `selector` under the cache `root`, and when its stack
/// was last resolved from its source.
pub fn load(root: &Path, selector: &Selector) -> Option<(Snapshot, SystemTime)> {
    let path = path_for(root, selector);
    let text = fs::read_to_string(&path).ok()?;
    let snapshot: Snapshot = serde_json::from_str(&text).ok()?;
    if snapshot.schema > SCHEMA || snapshot.selector != selector.key() {
        return None;
    }
    let confirmed = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()?;
    Some((snapshot, confirmed))
}

/// Keep `snapshot` for `selector`. It goes to a temporary file first and is
/// renamed into place, so a reader never sees half a file.
pub fn save(root: &Path, selector: &Selector, snapshot: &Snapshot) -> io::Result<()> {
    let folder = folder(root);
    fs::create_dir_all(&folder)?;
    let path = folder.join(selector.file_name());
    let temporary = temporary_for(&path);

    let text = serde_json::to_string_pretty(snapshot).map_err(io::Error::other)?;
    fs::write(&temporary, text)?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// A name for the file a save is written to before it replaces `path`. It is
/// different for every save, so envoys in other processes and threads in this
/// one, saving the same snapshot at once, never write into each other's file.
fn temporary_for(path: &Path) -> PathBuf {
    static SAVES: AtomicU64 = AtomicU64::new(0);
    path.with_extension(format!(
        "tmp{}-{}",
        std::process::id(),
        SAVES.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A named stack that has run on this machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Remembered {
    /// The stack's name.
    pub name: String,
    /// The publication version it had when it was last resolved.
    pub registry_version: Option<String>,
    /// When it was last resolved from its source.
    pub confirmed: SystemTime,
}

/// The named stacks remembered under the cache `root`, by name.
pub fn remembered(root: &Path) -> Vec<Remembered> {
    let Ok(entries) = fs::read_dir(folder(root)) else {
        return Vec::new();
    };

    let mut found: Vec<Remembered> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let text = fs::read_to_string(entry.path()).ok()?;
            let snapshot: Snapshot = serde_json::from_str(&text).ok()?;
            if snapshot.schema > SCHEMA || !snapshot.selector.starts_with("name:") {
                return None;
            }
            Some(Remembered {
                name: snapshot.name,
                registry_version: snapshot.registry_version,
                confirmed: entry.metadata().ok()?.modified().ok()?,
            })
        })
        .collect();
    found.sort_by(|left, right| left.name.cmp(&right.name));
    found
}

/// Note that the stack was resolved from its source just now, at most once an
/// hour. Best effort: a read-only snapshot still serves.
pub fn confirm(root: &Path, selector: &Selector) {
    let path = path_for(root, selector);
    let recent = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < CONFIRM_EVERY);
    if recent {
        return;
    }
    if let Ok(file) = fs::OpenOptions::new().write(true).open(&path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn snapshot_of(selector: &Selector) -> Snapshot {
        let mut snapshot = Snapshot::new(
            selector,
            PathBuf::from("/share/studio/2026-09-27/studio.estack"),
            Some(FileStamp {
                len: 120,
                modified: Some(1_700_000_000),
            }),
        );
        snapshot.name = "studio".to_string();
        snapshot.namespace = "gt".to_string();
        snapshot.registry_version = Some("2026-09-27".to_string());
        snapshot.bundles = vec![
            SnapshotBundle {
                path: PathBuf::from("/share/bndl/ext/python/3.11.9-envoy.2"),
                namespace: "ext".to_string(),
                name: "python".to_string(),
                version: Some("3.11.9-envoy.2".to_string()),
                metadata: HashMap::from([("role".to_string(), Value::from("core"))]),
            },
            SnapshotBundle {
                path: PathBuf::from("/work/dev"),
                namespace: "gt".to_string(),
                name: "dev".to_string(),
                version: None,
                metadata: HashMap::new(),
            },
        ];
        snapshot
    }

    #[test]
    fn a_saved_snapshot_is_found_again_exactly() {
        let root = tempdir().expect("tempdir is created");
        let selector = Selector::Name("studio".to_string());
        assert!(load(root.path(), &selector).is_none());

        let snapshot = snapshot_of(&selector);
        save(root.path(), &selector, &snapshot).expect("it is saved");

        let (found, confirmed) = load(root.path(), &selector).expect("it is found");
        assert_eq!(found, snapshot);
        assert!(confirmed.elapsed().expect("in the past") < Duration::from_secs(60));
        assert_eq!(found.bundles[0].label(), "ext:python@3.11.9-envoy.2");
        assert_eq!(found.bundles[1].label(), "gt:dev");
    }

    #[test]
    fn selectors_never_share_a_snapshot() {
        let root = tempdir().expect("tempdir is created");
        let studio = Selector::Name("studio".to_string());
        let build = Selector::Name("build".to_string());
        let file = Selector::Path(PathBuf::from("/stacks/studio.estack"));
        for selector in [&studio, &build, &file] {
            save(root.path(), selector, &snapshot_of(selector)).expect("it is saved");
        }

        assert_eq!(
            folder(root.path())
                .read_dir()
                .expect("folder reads")
                .count(),
            3
        );
        for selector in [&studio, &build, &file] {
            let (found, _) = load(root.path(), selector).expect("each is found");
            assert_eq!(found.selector, selector.key());
        }
    }

    #[test]
    fn a_file_written_for_another_selector_or_a_newer_layout_is_ignored() {
        let root = tempdir().expect("tempdir is created");
        let selector = Selector::Name("studio".to_string());
        let mut snapshot = snapshot_of(&selector);

        snapshot.selector = Selector::Name("other".to_string()).key();
        save(root.path(), &selector, &snapshot).expect("it is saved");
        assert!(
            load(root.path(), &selector).is_none(),
            "a clash is not trusted"
        );

        snapshot = snapshot_of(&selector);
        snapshot.schema = SCHEMA + 1;
        save(root.path(), &selector, &snapshot).expect("it is saved");
        assert!(
            load(root.path(), &selector).is_none(),
            "a newer layout is not read"
        );

        fs::write(path_for(root.path(), &selector), "{ not json").expect("file is damaged");
        assert!(
            load(root.path(), &selector).is_none(),
            "a damaged file is not an error"
        );
    }

    #[test]
    fn every_save_is_written_to_a_file_of_its_own_before_it_replaces_the_snapshot() {
        let path = Path::new("studio-0123456789abcdef.json");

        let first = temporary_for(path);
        let second = temporary_for(path);

        assert_ne!(first, second);
        assert_eq!(first.parent(), path.parent());
        for temporary in [&first, &second] {
            let extension = temporary.extension().and_then(|ext| ext.to_str());
            assert!(
                extension.is_some_and(|ext| ext.starts_with("tmp")),
                "{temporary:?} must not look like a snapshot to `remembered`"
            );
        }
    }

    #[test]
    fn saving_again_replaces_the_file_whole_and_leaves_nothing_behind() {
        let root = tempdir().expect("tempdir is created");
        let selector = Selector::Name("studio".to_string());
        save(root.path(), &selector, &snapshot_of(&selector)).expect("first save");

        let mut changed = snapshot_of(&selector);
        changed.registry_version = Some("2026-10-01".to_string());
        save(root.path(), &selector, &changed).expect("second save");

        let (found, _) = load(root.path(), &selector).expect("it is found");
        assert_eq!(found.registry_version.as_deref(), Some("2026-10-01"));
        assert_eq!(
            folder(root.path())
                .read_dir()
                .expect("folder reads")
                .count(),
            1
        );
    }

    #[test]
    fn a_snapshot_is_current_only_for_the_same_file_in_the_same_state() {
        let selector = Selector::Name("studio".to_string());
        let snapshot = snapshot_of(&selector);
        let stamp = snapshot.stamp;

        assert!(snapshot.is_of(&snapshot.path, stamp));
        assert!(!snapshot.is_of(Path::new("/elsewhere.estack"), stamp));
        assert!(!snapshot.is_of(
            &snapshot.path,
            Some(FileStamp {
                len: 121,
                modified: Some(1_700_000_000)
            })
        ));
        assert!(
            !snapshot.is_of(&snapshot.path, None),
            "an unreadable file is never current"
        );
    }

    #[test]
    fn confirming_refreshes_the_time_at_most_once_an_hour() {
        let root = tempdir().expect("tempdir is created");
        let selector = Selector::Name("studio".to_string());
        save(root.path(), &selector, &snapshot_of(&selector)).expect("it is saved");
        let path = path_for(root.path(), &selector);
        let backdate = |by: Duration| {
            fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(SystemTime::now() - by))
                .expect("the file is backdated");
        };
        let age = || {
            load(root.path(), &selector)
                .expect("it is found")
                .1
                .elapsed()
                .expect("in the past")
        };

        backdate(Duration::from_secs(10 * 60));
        confirm(root.path(), &selector);
        assert!(
            age() >= Duration::from_secs(9 * 60),
            "a recent look is left alone"
        );

        backdate(Duration::from_secs(3 * 3_600));
        confirm(root.path(), &selector);
        assert!(age() < Duration::from_secs(60), "an old one is refreshed");
    }

    #[test]
    fn remembered_lists_named_stacks_by_name_and_ignores_the_rest() {
        let root = tempdir().expect("tempdir is created");
        let studio = Selector::Name("studio".to_string());
        let build = Selector::Name("build".to_string());
        let file = Selector::Path(PathBuf::from("/stacks/other.estack"));
        for (selector, name) in [(&studio, "studio"), (&build, "build"), (&file, "other")] {
            let mut snapshot = snapshot_of(selector);
            snapshot.name = name.to_string();
            save(root.path(), selector, &snapshot).expect("it is saved");
        }
        fs::write(folder(root.path()).join("junk.json"), "{ not json").expect("junk is written");

        let found = remembered(root.path());

        assert_eq!(
            found
                .iter()
                .map(|stack| stack.name.as_str())
                .collect::<Vec<_>>(),
            vec!["build", "studio"]
        );
        assert_eq!(found[1].registry_version.as_deref(), Some("2026-09-27"));
        assert!(remembered(&root.path().join("nowhere")).is_empty());
    }

    #[test]
    fn a_path_is_told_from_a_name_and_a_description_is_for_people() {
        assert_eq!(
            Selector::parse("studio"),
            Selector::Name("studio".to_string())
        );
        assert!(matches!(
            Selector::parse("./studio.estack"),
            Selector::Path(_)
        ));
        assert!(matches!(
            Selector::parse("R:/stacks/studio.estack"),
            Selector::Path(_)
        ));

        let info = SnapshotInfo {
            confirmed: SystemTime::now() - Duration::from_secs(3 * 3_600 + 5),
            reason: "the stack roots in ENVOY_STACK_ROOTS cannot be reached".to_string(),
            unavailable: vec!["ext:qt@6.0".to_string(), "gt:tools@1".to_string()],
        };
        let lines = info.describe("studio", Some("2026-09-27"));

        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "envoy: the stack roots in ENVOY_STACK_ROOTS cannot be reached; using stack \
'studio' (published 2026-09-27) as it was 3 hours ago"
        );
        assert_eq!(
            lines[1],
            "envoy: not cached on this machine, so unavailable: ext:qt@6.0, gt:tools@1"
        );
        let plain = SnapshotInfo {
            unavailable: Vec::new(),
            ..info
        };
        assert_eq!(plain.describe("studio", None).len(), 1);
    }
}
