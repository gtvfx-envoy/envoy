use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{io_error, Result};

/// Layout version of the store, recorded in `store.json` and in every entry.
///
/// A store or entry with a larger number was written by a newer envoy and is
/// left alone rather than reinterpreted.
pub const STORE_FORMAT: u32 = 1;

/// Directory inside an entry that holds the store's own bookkeeping.
pub(crate) const STATE_DIR: &str = ".store";
const STATE_FILE: &str = "state.json";
const LAST_USED_FILE: &str = "last-used";
const KEEP_FILE: &str = "keep";

/// Last-used is refreshed at most this often, so reusing an entry costs one
/// `stat` rather than a write.
const TOUCH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// What the store records about a cached entry, in `.store/state.json`.
///
/// The file is written into the staged copy before it is renamed into place,
/// so an entry that has this file is complete.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EntryState {
    /// Layout version the entry was written with.
    pub format: u32,
    /// `true` once every file was copied and verified.
    pub complete: bool,
    /// Total size of the copied files, in bytes.
    pub bytes: u64,
    /// Number of files copied.
    pub files: u64,
    /// Where the entry was copied from.
    pub source: String,
    /// Unix time the entry was filled.
    pub filled_at: u64,
    /// Version of envoy that filled the entry.
    pub created_by: String,
}

pub(crate) fn state_dir(entry: &Path) -> PathBuf {
    entry.join(STATE_DIR)
}

/// Record `state` inside a staged entry, and create its last-used stamp.
pub(crate) fn write_state(staged_entry: &Path, state: &EntryState) -> Result<()> {
    let directory = state_dir(staged_entry);
    fs::create_dir_all(&directory).map_err(io_error(&directory))?;

    let state_path = directory.join(STATE_FILE);
    let text = serde_json::to_string_pretty(state).expect("entry state always serializes");
    fs::write(&state_path, text).map_err(io_error(&state_path))?;

    let stamp = directory.join(LAST_USED_FILE);
    File::create(&stamp).map_err(io_error(&stamp))?;
    Ok(())
}

/// Read an entry's state without judging whether this envoy can use it.
pub(crate) fn read_state(entry: &Path) -> Option<EntryState> {
    let text = fs::read_to_string(state_dir(entry).join(STATE_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Return `true` when this envoy can run from the entry as it is.
pub(crate) fn is_usable(state: &EntryState) -> bool {
    state.complete && state.format == STORE_FORMAT
}

/// Return when the entry was last used.
pub(crate) fn last_used(entry: &Path, state: Option<&EntryState>) -> SystemTime {
    fs::metadata(state_dir(entry).join(LAST_USED_FILE))
        .and_then(|metadata| metadata.modified())
        .ok()
        .or_else(|| state.map(|state| UNIX_EPOCH + Duration::from_secs(state.filled_at)))
        .unwrap_or(UNIX_EPOCH)
}

/// Mark the entry as used now, unless it was marked recently.
///
/// Best effort: a read-only store still serves its entries.
pub(crate) fn touch_last_used(entry: &Path, now: SystemTime) {
    let stamp = state_dir(entry).join(LAST_USED_FILE);
    let recent = fs::metadata(&stamp)
        .and_then(|metadata| metadata.modified())
        .map(|modified| now.duration_since(modified).unwrap_or(Duration::ZERO) < TOUCH_INTERVAL)
        .unwrap_or(false);
    if recent {
        return;
    }

    if let Ok(file) = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&stamp)
    {
        let _ = file.set_modified(now);
    }
}

/// Return `true` when the entry carries a marker asking prune to leave it.
pub(crate) fn is_kept(entry: &Path) -> bool {
    state_dir(entry).join(KEEP_FILE).exists()
}

/// Seconds since the Unix epoch.
pub(crate) fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
