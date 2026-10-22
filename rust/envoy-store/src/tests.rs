use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Barrier;
use std::time::{Duration, SystemTime};

use tempfile::{tempdir, TempDir};

use crate::refusal::{self, Refusal};
use crate::{EntryKey, Observer, Outcome, PrunePolicy, Quiet, Store, StoreError, STORE_FORMAT};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn key(version: &str) -> EntryKey {
    EntryKey::new("ext", "python", version).expect("test key is valid")
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("file has a parent")).expect("parent is created");
    fs::write(path, contents).expect("file is written");
}

/// A small bundle: four files, 14 bytes, plus an empty directory.
fn make_source(parent: &Path) -> PathBuf {
    let source = parent.join("source");
    write(&source.join(".bundle"), "{}");
    write(&source.join(".envoy/commands.json"), "{}");
    write(&source.join("lib/a/one.py"), "one");
    write(&source.join("lib/a/b/two.py"), "two two");
    fs::create_dir_all(source.join("empty/dir")).expect("empty directory is created");
    source
}

struct Fixture {
    _temp: TempDir,
    store: Store,
    source: PathBuf,
}

fn fixture() -> Fixture {
    let temp = tempdir().expect("tempdir is created");
    let store = Store::open(temp.path().join("store")).expect("store opens");
    let source = make_source(temp.path());
    Fixture {
        _temp: temp,
        store,
        source,
    }
}

/// Counts the events a fill reports.
#[derive(Default)]
struct Counting {
    started: AtomicU32,
    waited: AtomicU32,
}

impl Observer for Counting {
    fn waiting(&self, _key: &EntryKey) {
        self.waited.fetch_add(1, Ordering::SeqCst);
    }

    fn started(&self, _key: &EntryKey, _files: u64, _bytes: u64) {
        self.started.fetch_add(1, Ordering::SeqCst);
    }
}

/// Backdate an entry's last-used stamp.
fn set_last_used(entry: &Path, age: Duration) {
    let stamp = entry.join(".store").join("last-used");
    File::options()
        .write(true)
        .open(stamp)
        .expect("stamp opens")
        .set_modified(SystemTime::now() - age)
        .expect("stamp is backdated");
}

fn read_tree(root: &Path) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("directory reads") {
            let path = entry.expect("entry reads").path();
            let relative = path
                .strip_prefix(root)
                .expect("path is under root")
                .to_string_lossy()
                .replace('\\', "/");
            if relative.starts_with(".store") {
                continue;
            }
            if path.is_dir() {
                found.push((format!("{relative}/"), String::new()));
                pending.push(path);
            } else {
                found.push((relative, fs::read_to_string(&path).unwrap_or_default()));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn ensure_copies_a_bundle_into_the_store() {
    let fixture = fixture();
    let key = key("3.11.9-envoy.2");

    let ensured = fixture
        .store
        .ensure(&key, &fixture.source, &Quiet)
        .expect("ensure fills");

    assert_eq!(ensured.path, fixture.store.entry_path(&key));
    match ensured.outcome {
        Outcome::Filled { files, bytes, .. } => assert_eq!((files, bytes), (4, 14)),
        Outcome::Hit => panic!("a first ensure must fill"),
    }
    assert_eq!(read_tree(&ensured.path), read_tree(&fixture.source));

    let state = fixture.store.lookup(&key).expect("entry is cached");
    assert!(state.complete);
    assert_eq!(state.format, STORE_FORMAT);
    assert_eq!((state.files, state.bytes), (4, 14));
}

#[test]
fn ensure_reuses_a_cached_entry_without_reading_the_source() {
    let fixture = fixture();
    let key = key("1.0.0");
    drop(fixture.store.ensure(&key, &fixture.source, &Quiet).unwrap());

    fs::remove_dir_all(&fixture.source).expect("source is removed");
    let again = fixture
        .store
        .ensure(&key, &fixture.source, &Quiet)
        .expect("a cached entry needs no source");

    assert_eq!(again.outcome, Outcome::Hit);
}

#[test]
fn versions_of_one_bundle_are_cached_separately() {
    let fixture = fixture();
    let first = fixture
        .store
        .ensure(&key("1.0.0"), &fixture.source, &Quiet)
        .unwrap();
    let second = fixture
        .store
        .ensure(&key("v1.0.0"), &fixture.source, &Quiet)
        .unwrap();

    assert_ne!(first.path, second.path);
    assert_eq!(fixture.store.list().len(), 2);
}

#[test]
fn a_directory_without_a_record_is_replaced_not_served() {
    let fixture = fixture();
    let key = key("1.0.0");
    let entry = fixture.store.entry_path(&key);
    write(&entry.join("leftover.txt"), "from a crash");
    assert!(fixture.store.lookup(&key).is_none());

    let ensured = fixture.store.ensure(&key, &fixture.source, &Quiet).unwrap();

    assert!(matches!(ensured.outcome, Outcome::Filled { .. }));
    assert!(!entry.join("leftover.txt").exists());
    assert!(entry.join("lib/a/one.py").is_file());
}

#[test]
fn a_failed_fill_leaves_no_entry_and_no_staging() {
    let fixture = fixture();
    let key = key("1.0.0");
    let missing = fixture.source.join("does-not-exist");

    let error = fixture
        .store
        .ensure(&key, &missing, &Quiet)
        .expect_err("a missing source cannot be cached");

    assert!(matches!(error, StoreError::InvalidSource { .. }));
    assert!(fixture.store.lookup(&key).is_none());
    assert!(!fixture.store.entry_path(&key).exists());
}

#[test]
fn a_copy_that_fails_partway_publishes_nothing() {
    let fixture = fixture();
    let key = key("1.0.0");
    let locked = fixture.source.join("lib/a/one.py");

    // Make one file unreadable after the plan could still see it.
    #[cfg(windows)]
    let _hold = {
        use std::os::windows::fs::OpenOptionsExt;
        File::options()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .expect("file is locked")
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
        if File::open(&locked).is_ok() {
            // Running as root: permissions cannot make a file unreadable.
            return;
        }
    }

    let result = fixture.store.ensure(&key, &fixture.source, &Quiet);

    assert!(result.is_err(), "the unreadable file must fail the fill");
    assert!(fixture.store.lookup(&key).is_none());
    assert!(!fixture.store.entry_path(&key).exists());
    let staging = fixture.store.root().join(".staging");
    let leftovers = fs::read_dir(&staging).map(|entries| entries.count());
    assert_eq!(leftovers.unwrap_or(0), 0, "staging must be cleaned up");
}

#[test]
fn concurrent_ensures_fill_an_entry_exactly_once() {
    let fixture = fixture();
    for index in 0..300 {
        write(
            &fixture.source.join(format!("many/file_{index:03}.txt")),
            "x",
        );
    }
    let key = key("1.0.0");
    let observer = Counting::default();
    let barrier = Barrier::new(6);

    let paths: Vec<PathBuf> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..6)
            .map(|_| {
                scope.spawn(|| {
                    // Each thread opens its own handle, as separate processes would.
                    let store = Store::open(fixture.store.root()).expect("store opens");
                    barrier.wait();
                    store
                        .ensure(&key, &fixture.source, &observer)
                        .expect("ensure succeeds")
                        .path
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread finishes"))
            .collect()
    });

    assert_eq!(observer.started.load(Ordering::SeqCst), 1);
    assert!(paths.iter().all(|path| path == &paths[0]));
    assert_eq!(
        fixture.store.lookup(&key).expect("cached").files,
        304,
        "the entry must be complete, not a partial copy"
    );
}

#[test]
fn reusing_an_entry_refreshes_a_stale_last_used_stamp() {
    let fixture = fixture();
    let key = key("1.0.0");
    let entry = fixture.store.ensure(&key, &fixture.source, &Quiet).unwrap();
    set_last_used(&entry.path, 2 * DAY);
    let before = fixture.store.list()[0].last_used;

    drop(fixture.store.ensure(&key, &fixture.source, &Quiet).unwrap());

    assert!(fixture.store.list()[0].last_used > before + DAY);
}

#[test]
fn prune_removes_only_entries_idle_longer_than_the_limit() {
    let fixture = fixture();
    let old = fixture
        .store
        .ensure(&key("1.0.0"), &fixture.source, &Quiet)
        .unwrap();
    let fresh = fixture
        .store
        .ensure(&key("2.0.0"), &fixture.source, &Quiet)
        .unwrap();
    set_last_used(&old.path, 31 * DAY);
    set_last_used(&fresh.path, 29 * DAY);
    let old_path = old.path.clone();
    drop((old, fresh));

    let report = fixture.store.prune(&PrunePolicy::idle_for(30 * DAY));

    assert_eq!(report.removed, vec![key("1.0.0")]);
    assert_eq!(report.freed_bytes, 14);
    assert!(!old_path.exists());
    assert!(fixture.store.lookup(&key("2.0.0")).is_some());
    let trash = fs::read_dir(fixture.store.root().join(".trash"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(trash, 0, "removed entries are deleted, not left in trash");
}

#[test]
fn prune_leaves_an_entry_that_is_in_use() {
    let fixture = fixture();
    let key = key("1.0.0");
    let in_use = fixture.store.ensure(&key, &fixture.source, &Quiet).unwrap();
    set_last_used(&in_use.path, 40 * DAY);

    let blocked = fixture.store.prune(&PrunePolicy::idle_for(30 * DAY));
    assert!(blocked.removed.is_empty());
    assert_eq!(blocked.in_use, vec![key.clone()]);
    assert!(fixture.store.lookup(&key).is_some());

    drop(in_use);
    let released = fixture.store.prune(&PrunePolicy::idle_for(30 * DAY));
    assert_eq!(released.removed, vec![key.clone()]);
    assert!(fixture.store.lookup(&key).is_none());
}

#[test]
fn prune_honours_a_keep_marker_unless_told_to_ignore_it() {
    let fixture = fixture();
    let key = key("1.0.0");
    let entry = fixture.store.ensure(&key, &fixture.source, &Quiet).unwrap();
    set_last_used(&entry.path, 40 * DAY);
    write(&entry.path.join(".store/keep"), "");
    drop(entry);

    assert!(fixture
        .store
        .prune(&PrunePolicy::idle_for(30 * DAY))
        .removed
        .is_empty());

    let policy = PrunePolicy {
        ignore_keep: true,
        ..PrunePolicy::idle_for(30 * DAY)
    };
    assert_eq!(fixture.store.prune(&policy).removed, vec![key]);
}

#[test]
fn prune_with_a_size_limit_evicts_the_least_recently_used_first() {
    let fixture = fixture();
    for (version, age) in [("1.0.0", 3), ("2.0.0", 2), ("3.0.0", 1)] {
        let entry = fixture
            .store
            .ensure(&key(version), &fixture.source, &Quiet)
            .unwrap();
        set_last_used(&entry.path, age * DAY);
    }

    // Three entries of 14 bytes; a 20 byte limit leaves room for one.
    let policy = PrunePolicy {
        max_bytes: Some(20),
        ..PrunePolicy::idle_for(30 * DAY)
    };
    let report = fixture.store.prune(&policy);

    assert_eq!(report.removed, vec![key("1.0.0"), key("2.0.0")]);
    assert!(fixture.store.lookup(&key("3.0.0")).is_some());
}

#[test]
fn a_dry_run_reports_without_removing() {
    let fixture = fixture();
    let entry = fixture
        .store
        .ensure(&key("1.0.0"), &fixture.source, &Quiet)
        .unwrap();
    set_last_used(&entry.path, 40 * DAY);
    drop(entry);

    let policy = PrunePolicy {
        dry_run: true,
        ..PrunePolicy::idle_for(30 * DAY)
    };
    let report = fixture.store.prune(&policy);

    assert_eq!(report.removed, vec![key("1.0.0")]);
    assert!(fixture.store.lookup(&key("1.0.0")).is_some());
}

#[test]
fn clear_removes_every_entry_that_is_not_in_use() {
    let fixture = fixture();
    drop(
        fixture
            .store
            .ensure(&key("1.0.0"), &fixture.source, &Quiet)
            .unwrap(),
    );
    let held = fixture
        .store
        .ensure(&key("2.0.0"), &fixture.source, &Quiet)
        .unwrap();

    let report = fixture.store.clear();

    assert_eq!(report.removed, vec![key("1.0.0")]);
    assert_eq!(report.in_use, vec![key("2.0.0")]);
    drop(held);
}

#[test]
fn orphaned_staging_and_trash_are_cleaned_up() {
    let fixture = fixture();
    let staging = fixture.store.root().join(".staging/dead-fill");
    let trash = fixture.store.root().join(".trash/dead-removal");
    write(&staging.join("half-copied.bin"), "partial");
    write(&trash.join("old.bin"), "old");

    fixture.store.clean_orphans_older_than(Duration::ZERO);

    assert!(!staging.exists());
    assert!(!trash.exists());
}

#[test]
fn a_store_or_entry_from_a_newer_envoy_is_left_alone() {
    let fixture = fixture();
    let newer = fixture.store.root().join("store.json");
    fs::write(&newer, r#"{"format": 99, "created_by": "9.9.9"}"#).expect("marker is written");
    assert!(matches!(
        Store::open(fixture.store.root()),
        Err(StoreError::UnsupportedFormat { found: 99, .. })
    ));
    fs::write(&newer, r#"{"format": 1, "created_by": "0.0.0"}"#).expect("marker is restored");

    let key = key("1.0.0");
    let entry = fixture.store.entry_path(&key);
    write(
        &entry.join(".store/state.json"),
        r#"{"format": 99, "complete": true, "bytes": 1, "files": 1, "source": "x", "filled_at": 0, "created_by": "9.9.9"}"#,
    );

    assert!(matches!(
        fixture.store.ensure(&key, &fixture.source, &Quiet),
        Err(StoreError::UnsupportedFormat { found: 99, .. })
    ));
    let mut policy = PrunePolicy::everything();
    policy.max_idle = Duration::ZERO;
    assert!(fixture.store.prune(&policy).removed.is_empty());
    assert!(entry.exists());
}

#[cfg(windows)]
/// A store whose root is too deep for the source's deepest file to fit under it.
fn too_deep_for_the_store(temp: &Path) -> (Store, PathBuf) {
    let root = (0..9).fold(temp.join("store"), |path, level| {
        path.join(format!("deep_root_{level}"))
    });
    let store = Store::open(&root).expect("store opens");
    let source = temp.join("source");
    let deep = (0..12).fold(source.clone(), |path, level| {
        path.join(format!("nested_{level:05}"))
    });
    write(&deep.join("leaf.txt"), "deep");
    (store, source)
}

#[cfg(windows)]
#[test]
fn an_entry_whose_paths_would_be_too_long_is_refused() {
    let temp = tempdir().expect("tempdir is created");
    let (store, source) = too_deep_for_the_store(temp.path());

    let error = store
        .ensure(&key("1.0.0"), &source, &Quiet)
        .expect_err("paths over the limit are refused");

    assert!(
        matches!(
            error,
            StoreError::PathTooLong {
                remembered: false,
                ..
            }
        ),
        "{error}"
    );
    assert!(store.lookup(&key("1.0.0")).is_none());
}

#[cfg(windows)]
#[test]
fn a_refused_entry_is_not_walked_again() {
    let temp = tempdir().expect("tempdir is created");
    let (store, source) = too_deep_for_the_store(temp.path());
    let key = key("1.0.0");
    store
        .ensure(&key, &source, &Quiet)
        .expect_err("the first run finds the entry does not fit");

    // With the source gone, only a remembered answer can explain a refusal.
    fs::remove_dir_all(&source).expect("source is removed");
    let error = store
        .ensure(&key, &source, &Quiet)
        .expect_err("the refusal is remembered");

    assert!(
        matches!(
            error,
            StoreError::PathTooLong {
                remembered: true,
                ..
            }
        ),
        "{error}"
    );
}

#[cfg(windows)]
/// A source whose one file has a path of exactly `longest` characters.
fn source_with_longest_path(parent: &Path, longest: usize) -> PathBuf {
    let source = parent.join("source");
    write(&source.join("d".repeat(longest - 2)).join("f"), "x");
    source
}

#[cfg(windows)]
#[test]
fn the_path_limit_is_measured_at_the_final_location() {
    let temp = tempdir().expect("tempdir is created");
    let store = Store::open(temp.path().join("store")).expect("store opens");
    let fits = EntryKey::new("a", "b", "1").expect("key is valid");
    let over = EntryKey::new("a", "b", "2").expect("key is valid");
    let longest = 250 - (store.entry_path(&fits).as_os_str().len() + 1);
    assert!(
        longest > 2 && longest < 255,
        "the temp dir is unusable here"
    );

    // Only the final location counts. A staging path is longer than it by a few
    // characters here, and still works because std gives it the long-path prefix.
    let at_limit = source_with_longest_path(&temp.path().join("fits"), longest);
    store
        .ensure(&fits, &at_limit, &Quiet)
        .expect("a path exactly at the limit is cached");

    let too_long = source_with_longest_path(&temp.path().join("over"), longest + 1);
    let error = store
        .ensure(&over, &too_long, &Quiet)
        .expect_err("a path one character over is refused");
    assert!(
        matches!(
            error,
            StoreError::PathTooLong {
                needed: 251,
                limit: 250,
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn a_refusal_that_no_longer_holds_is_forgotten() {
    let fixture = fixture();
    let key = key("1.0.0");
    let record = fixture.store.refusal_path(&key);
    refusal::write(&record, &Refusal::path_too_long(5)).expect("record is written");

    let ensured = fixture
        .store
        .ensure(&key, &fixture.source, &Quiet)
        .expect("a bundle that fits is cached");

    assert!(matches!(ensured.outcome, Outcome::Filled { .. }));
    assert!(!record.exists());
}

#[test]
fn prune_forgets_idle_refusals_and_clear_forgets_them_all() {
    let fixture = fixture();
    let old = fixture.store.refusal_path(&key("1.0.0"));
    let recent = fixture.store.refusal_path(&key("2.0.0"));
    for record in [&old, &recent] {
        refusal::write(record, &Refusal::path_too_long(300)).expect("record is written");
    }
    File::options()
        .write(true)
        .open(&old)
        .expect("record opens")
        .set_modified(SystemTime::now() - 40 * DAY)
        .expect("record is backdated");

    fixture.store.prune(&PrunePolicy::idle_for(30 * DAY));
    assert!(!old.exists());
    assert!(recent.exists());

    let mut dry_run = PrunePolicy::everything();
    dry_run.dry_run = true;
    fixture.store.prune(&dry_run);
    assert!(recent.exists());

    fixture.store.clear();
    assert!(!recent.exists());
}

#[cfg(unix)]
#[test]
fn symbolic_links_are_recreated_as_links() {
    let fixture = fixture();
    std::os::unix::fs::symlink("one.py", fixture.source.join("lib/a/link.py")).expect("symlink");
    let key = key("1.0.0");

    let ensured = fixture.store.ensure(&key, &fixture.source, &Quiet).unwrap();

    let link = ensured.path.join("lib/a/link.py");
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("one.py"));
}

#[cfg(unix)]
#[test]
fn file_permissions_survive_the_copy() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixture();
    let tool = fixture.source.join("bin/tool");
    write(&tool, "#!/bin/sh\n");
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).expect("chmod");

    let ensured = fixture
        .store
        .ensure(&key("1.0.0"), &fixture.source, &Quiet)
        .unwrap();

    let mode = fs::metadata(ensured.path.join("bin/tool"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
}
