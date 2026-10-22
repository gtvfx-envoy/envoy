use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{io_error, Result, StoreError};
use crate::fill;
use crate::key::EntryKey;
use crate::lock::{Lease, LockFile};
use crate::observer::Observer;
use crate::state::{
    self, is_kept, is_usable, last_used, read_state, touch_last_used, unix_seconds, EntryState,
    STORE_FORMAT,
};

const STORE_FILE: &str = "store.json";
const BUNDLES_DIR: &str = "bundles";
const STAGING_DIR: &str = ".staging";
const TRASH_DIR: &str = ".trash";
const LOCKS_DIR: &str = "locks";

/// Staging and trash directories older than this belong to a dead process.
const ORPHAN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Free space kept in reserve beyond the size of a fill.
const SPACE_HEADROOM: u64 = 64 * 1024 * 1024;

/// Longest path (in characters) the store will create on Windows, leaving
/// room below the classic 260 limit that many tools still honour.
const WINDOWS_PATH_LIMIT: usize = 250;

/// Room reserved for a staging directory's unique name.
const STAGING_NAME_ALLOWANCE: usize = 32;

/// Times `ensure` will start over when an entry is removed under it.
const ENSURE_ATTEMPTS: u32 = 3;

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Deserialize, Serialize)]
struct StoreMarker {
    format: u32,
    created_by: String,
}

/// How an entry came to be usable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// It was already cached.
    Hit,
    /// It was copied in just now.
    Filled {
        /// Files copied.
        files: u64,
        /// Bytes copied.
        bytes: u64,
        /// Time the fill took.
        elapsed: Duration,
    },
}

/// A cached entry ready to run from.
#[derive(Debug)]
pub struct Ensured {
    /// Root of the cached bundle; use this in place of the source path.
    pub path: PathBuf,
    /// Whether it was already cached or just filled.
    pub outcome: Outcome,
    /// Keeps prune away until dropped, or until the process exits.
    pub lease: Lease,
}

/// One entry found in the store.
#[derive(Clone, Debug)]
pub struct EntryInfo {
    /// The entry's identity.
    pub key: EntryKey,
    /// Where the entry lives.
    pub path: PathBuf,
    /// What the store recorded about it; `None` for a directory that has no
    /// readable record.
    pub state: Option<EntryState>,
    /// When it was last used.
    pub last_used: SystemTime,
    /// `true` when it carries a `keep` marker.
    pub keep: bool,
}

impl EntryInfo {
    /// Size recorded for the entry, in bytes.
    pub fn bytes(&self) -> u64 {
        self.state.as_ref().map_or(0, |state| state.bytes)
    }

    /// `true` when this envoy can run from the entry.
    pub fn usable(&self) -> bool {
        self.state.as_ref().is_some_and(is_usable)
    }
}

/// What [`Store::prune`] may remove.
#[derive(Clone, Debug)]
pub struct PrunePolicy {
    /// Entries unused for longer than this are removed.
    pub max_idle: Duration,
    /// When set, the least recently used entries are also removed until the
    /// store fits.
    pub max_bytes: Option<u64>,
    /// Remove entries even when they carry a `keep` marker.
    pub ignore_keep: bool,
    /// Report what would be removed without removing it.
    pub dry_run: bool,
    /// The time to measure idleness against.
    pub now: SystemTime,
}

impl PrunePolicy {
    /// Remove entries idle for longer than `max_idle`, with no size limit.
    pub fn idle_for(max_idle: Duration) -> Self {
        Self {
            max_idle,
            max_bytes: None,
            ignore_keep: false,
            dry_run: false,
            now: SystemTime::now(),
        }
    }

    /// Remove every entry that is not in use.
    pub fn everything() -> Self {
        Self {
            ignore_keep: true,
            ..Self::idle_for(Duration::ZERO)
        }
    }
}

/// What [`Store::prune`] did.
#[derive(Clone, Debug, Default)]
pub struct PruneReport {
    /// Entries removed (or that would be, in a dry run).
    pub removed: Vec<EntryKey>,
    /// Entries left because something is running from them.
    pub in_use: Vec<EntryKey>,
    /// Bytes recorded for the removed entries.
    pub freed_bytes: u64,
}

/// A name-addressed store of published bundles.
///
/// ```text
/// <root>/
///   store.json                         layout version
///   bundles/<ns>/<name>/<version>/     a bundle, ready to run
///     .store/                          state, last-used stamp, optional keep
///   locks/                             one lock file per entry
///   .staging/                          fills in progress
///   .trash/                            removals in progress
/// ```
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open the store at `root`, creating it when it does not exist yet.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(io_error(&root))?;

        let marker_path = root.join(STORE_FILE);
        match fs::read_to_string(&marker_path) {
            Ok(text) => {
                if let Ok(marker) = serde_json::from_str::<StoreMarker>(&text) {
                    if marker.format > STORE_FORMAT {
                        return Err(StoreError::UnsupportedFormat {
                            path: root,
                            found: marker.format,
                            supported: STORE_FORMAT,
                        });
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let marker = StoreMarker {
                    format: STORE_FORMAT,
                    created_by: env!("CARGO_PKG_VERSION").to_string(),
                };
                let text = serde_json::to_string_pretty(&marker).expect("marker serializes");
                fs::write(&marker_path, text).map_err(io_error(&marker_path))?;
            }
            Err(error) => return Err(io_error(&marker_path)(error)),
        }

        Ok(Self { root })
    }

    /// Return the store's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Return where an entry lives, whether or not it is cached.
    pub fn entry_path(&self, key: &EntryKey) -> PathBuf {
        self.root.join(key.relative_dir())
    }

    fn locks_dir(&self) -> PathBuf {
        self.root.join(LOCKS_DIR)
    }

    fn staging_dir(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }

    fn trash_dir(&self) -> PathBuf {
        self.root.join(TRASH_DIR)
    }

    fn lock_for(&self, key: &EntryKey) -> Result<LockFile> {
        LockFile::open(&self.locks_dir().join(key.lock_file_name()))
    }

    /// Return the state of a cached entry, or `None` if it is not cached.
    ///
    /// Does not touch the entry's last-used stamp, so looking is free.
    pub fn lookup(&self, key: &EntryKey) -> Option<EntryState> {
        read_state(&self.entry_path(key)).filter(is_usable)
    }

    fn reuse(&self, key: &EntryKey) -> bool {
        let entry = self.entry_path(key);
        let usable = read_state(&entry).is_some_and(|state| is_usable(&state));
        if usable {
            touch_last_used(&entry, SystemTime::now());
        }
        usable
    }

    /// Make `key` available locally, copying it from `source` when needed.
    ///
    /// A cached entry is returned as it is, with no walk and no hashing. On a
    /// miss the entry is copied into a staging directory next to the store and
    /// renamed into place, so a reader never sees a partial entry. Envoys that
    /// want the same entry at once take turns: one fills, the rest wait and
    /// then reuse it.
    ///
    /// The returned [`Lease`] keeps [`Store::prune`] away until it is dropped.
    pub fn ensure(
        &self,
        key: &EntryKey,
        source: &Path,
        observer: &dyn Observer,
    ) -> Result<Ensured> {
        let lock = self.lock_for(key)?;
        let mut filled = None;

        for _ in 0..ENSURE_ATTEMPTS {
            // Reading needs only a shared lock, so many envoys can run from
            // one entry together.
            if !lock.try_shared()? {
                observer.waiting(key);
                lock.shared()?;
            }
            if self.reuse(key) {
                return Ok(Ensured {
                    path: self.entry_path(key),
                    outcome: filled.unwrap_or(Outcome::Hit),
                    lease: lock.into_lease(),
                });
            }
            lock.unlock()?;

            // A lock cannot be upgraded in place, so fill under a fresh
            // exclusive one and check again: another envoy may have won.
            if !lock.try_exclusive()? {
                observer.waiting(key);
                lock.exclusive()?;
            }
            if !self.reuse(key) {
                filled = Some(self.fill(key, source, observer)?);
            }
            lock.unlock()?;
        }

        Err(StoreError::Io {
            path: self.entry_path(key),
            source: io::Error::other("the entry kept disappearing while it was being cached"),
        })
    }

    fn fill(&self, key: &EntryKey, source: &Path, observer: &dyn Observer) -> Result<Outcome> {
        if !source.is_dir() {
            return Err(StoreError::InvalidSource {
                key: key.to_string(),
                path: source.to_path_buf(),
            });
        }

        let started = Instant::now();
        let plan = fill::plan(source).map_err(io_error(source))?;
        self.check_path_budget(key, &plan)?;
        self.check_free_space(key, &plan)?;

        self.clean_orphans();
        observer.started(key, plan.files.len() as u64, plan.bytes);

        let staging = self.new_staging_path();
        let outcome = self.fill_into(key, source, &staging, &plan, observer);
        if outcome.is_err() {
            fill::remove_tree(&staging);
        }
        observer.finished(key, started.elapsed());

        outcome.map(|(files, bytes)| Outcome::Filled {
            files,
            bytes,
            elapsed: started.elapsed(),
        })
    }

    fn fill_into(
        &self,
        key: &EntryKey,
        source: &Path,
        staging: &Path,
        plan: &fill::Plan,
        observer: &dyn Observer,
    ) -> Result<(u64, u64)> {
        fill::copy_plan(source, staging, plan, key, observer)?;

        let expected_files = plan.files.len() as u64;
        let (found_files, found_bytes) = fill::measure(staging).map_err(io_error(staging))?;
        if found_files != expected_files || found_bytes != plan.bytes {
            return Err(StoreError::Incomplete {
                key: key.to_string(),
                expected_files,
                expected_bytes: plan.bytes,
                found_files,
                found_bytes,
            });
        }

        state::write_state(
            staging,
            &EntryState {
                format: STORE_FORMAT,
                complete: true,
                bytes: plan.bytes,
                files: expected_files,
                source: source.display().to_string(),
                filled_at: unix_seconds(SystemTime::now()),
                created_by: env!("CARGO_PKG_VERSION").to_string(),
            },
        )?;

        self.publish(key, staging)?;
        Ok((expected_files, plan.bytes))
    }

    /// Move a finished staging directory into place.
    fn publish(&self, key: &EntryKey, staging: &Path) -> Result<()> {
        let entry = self.entry_path(key);

        // We hold the entry's exclusive lock, so anything already here is a
        // leftover: a partial directory, or one this envoy cannot read.
        if let Some(existing) = read_state(&entry) {
            if existing.format > STORE_FORMAT {
                return Err(StoreError::UnsupportedFormat {
                    path: entry,
                    found: existing.format,
                    supported: STORE_FORMAT,
                });
            }
        }
        if entry.exists() {
            self.discard(&entry)?;
        }

        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent).map_err(io_error(parent))?;
        }
        fill::rename_with_retry(staging, &entry).map_err(io_error(&entry))
    }

    /// Move a directory out of the way, then delete it.
    fn discard(&self, directory: &Path) -> Result<()> {
        let trash = self.trash_dir();
        fs::create_dir_all(&trash).map_err(io_error(&trash))?;

        let target = trash.join(unique_name());
        fill::rename_with_retry(directory, &target).map_err(io_error(directory))?;
        fill::remove_tree(&target);
        Ok(())
    }

    fn new_staging_path(&self) -> PathBuf {
        self.staging_dir().join(unique_name())
    }

    fn check_path_budget(&self, key: &EntryKey, plan: &fill::Plan) -> Result<()> {
        if !cfg!(windows) {
            return Ok(());
        }

        let in_staging = self.staging_dir().as_os_str().len() + 1 + STAGING_NAME_ALLOWANCE;
        let in_place = self.entry_path(key).as_os_str().len();
        let needed = in_staging.max(in_place) + 1 + plan.longest;
        if needed > WINDOWS_PATH_LIMIT {
            return Err(StoreError::PathTooLong {
                key: key.to_string(),
                needed,
                limit: WINDOWS_PATH_LIMIT,
                root: self.root.clone(),
            });
        }
        Ok(())
    }

    fn check_free_space(&self, key: &EntryKey, plan: &fill::Plan) -> Result<()> {
        let needed = plan.bytes.saturating_add(SPACE_HEADROOM);
        match fs4::available_space(&self.root) {
            Ok(available) if available < needed => Err(StoreError::InsufficientSpace {
                key: key.to_string(),
                path: self.root.clone(),
                needed,
                available,
            }),
            // If the volume cannot be measured, try anyway.
            _ => Ok(()),
        }
    }

    /// Delete staging and trash directories a dead process left behind.
    fn clean_orphans(&self) {
        self.clean_orphans_older_than(ORPHAN_AGE);
    }

    pub(crate) fn clean_orphans_older_than(&self, age: Duration) {
        for directory in [self.staging_dir(), self.trash_dir()] {
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let old = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|elapsed| elapsed >= age);
                if old {
                    fill::remove_tree(&entry.path());
                }
            }
        }
    }

    /// List every entry in the store, ordered by identity.
    pub fn list(&self) -> Vec<EntryInfo> {
        let mut entries = Vec::new();

        for namespace in subdirectories(&self.root.join(BUNDLES_DIR)) {
            for name in subdirectories(&namespace) {
                for version in subdirectories(&name) {
                    let (Some(namespace_name), Some(bundle_name), Some(version_name)) =
                        (file_name(&namespace), file_name(&name), file_name(&version))
                    else {
                        continue;
                    };
                    let Ok(key) = EntryKey::new(&namespace_name, &bundle_name, &version_name)
                    else {
                        continue;
                    };

                    let state = read_state(&version);
                    entries.push(EntryInfo {
                        last_used: last_used(&version, state.as_ref()),
                        keep: is_kept(&version),
                        path: version,
                        state,
                        key,
                    });
                }
            }
        }

        entries.sort_by_key(|entry| entry.key.to_string());
        entries
    }

    /// Remove entries the policy allows, skipping any that are in use.
    ///
    /// An entry is in use while an envoy holds a [`Lease`] on it, and on
    /// Windows also while any process has a file open inside it.
    pub fn prune(&self, policy: &PrunePolicy) -> PruneReport {
        self.clean_orphans();

        let mut doomed = Vec::new();
        let mut survivors = Vec::new();
        for info in self.list() {
            let idle = policy
                .now
                .duration_since(info.last_used)
                .is_ok_and(|age| age > policy.max_idle);
            let newer_format = info
                .state
                .as_ref()
                .is_some_and(|state| state.format > STORE_FORMAT);
            let protected = (info.keep && !policy.ignore_keep) || newer_format;

            if idle && !protected {
                doomed.push(info);
            } else {
                survivors.push(info);
            }
        }

        if let Some(limit) = policy.max_bytes {
            survivors.sort_by_key(|info| info.last_used);
            let mut total: u64 = survivors.iter().map(EntryInfo::bytes).sum();
            let mut index = 0;
            while total > limit && index < survivors.len() {
                let candidate = &survivors[index];
                let protected = (candidate.keep && !policy.ignore_keep)
                    || candidate
                        .state
                        .as_ref()
                        .is_some_and(|state| state.format > STORE_FORMAT);
                if protected {
                    index += 1;
                } else {
                    total = total.saturating_sub(candidate.bytes());
                    doomed.push(survivors.remove(index));
                }
            }
        }

        let mut report = PruneReport::default();
        for info in doomed {
            if policy.dry_run {
                report.freed_bytes += info.bytes();
                report.removed.push(info.key);
                continue;
            }
            match self.remove_entry(&info) {
                Ok(true) => {
                    report.freed_bytes += info.bytes();
                    report.removed.push(info.key);
                }
                Ok(false) => report.in_use.push(info.key),
                Err(error) => {
                    tracing::debug!(entry = %info.key, "Could not remove entry: {error}");
                    report.in_use.push(info.key);
                }
            }
        }

        report
    }

    /// Remove every entry that is not in use.
    pub fn clear(&self) -> PruneReport {
        self.prune(&PrunePolicy::everything())
    }

    /// Remove one entry if nothing is using it. Returns `false` when it is
    /// in use.
    fn remove_entry(&self, info: &EntryInfo) -> Result<bool> {
        let lock = self.lock_for(&info.key)?;
        if !lock.try_exclusive()? {
            return Ok(false);
        }

        // On Windows the rename fails while a process has a file open inside
        // the entry, which also marks it as in use.
        let removed = match self.discard(&info.path) {
            Ok(()) => true,
            Err(StoreError::Io { source, .. }) if source.kind() != io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };

        lock.unlock()?;
        Ok(removed)
    }
}

fn unique_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{:x}{:x}{:x}",
        std::process::id(),
        nanos,
        STAGING_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn subdirectories(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut directories: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
        .map(|entry| entry.path())
        .collect();
    directories.sort();
    directories
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}
