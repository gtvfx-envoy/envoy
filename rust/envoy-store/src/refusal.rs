//! Remembering that an entry cannot be cached in a store.
//!
//! Whether an entry's paths fit under the store root depends on the shape of
//! its source tree, which takes a walk of the share to learn. The answer is
//! kept, so a refused entry costs nothing on later runs instead of a walk and
//! a warning each time.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::state::{unix_seconds, STORE_FORMAT};

/// Directory under the store root that holds one record per refused entry.
pub(crate) const REFUSED_DIR: &str = "refused";

/// What is remembered about a refused entry.
///
/// Only a fact about the source tree is stored. Whether it fits is decided
/// again from the store's current location and rules, so moving the store or
/// relaxing a rule lifts the refusal with no bookkeeping.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Refusal {
    /// Layout version the record was written with.
    pub(crate) format: u32,
    /// Length of the longest path in the source, relative to its root.
    pub(crate) longest: usize,
    /// Unix time the entry was first refused.
    pub(crate) recorded_at: u64,
    /// Version of envoy that recorded it.
    pub(crate) created_by: String,
}

impl Refusal {
    pub(crate) fn path_too_long(longest: usize) -> Self {
        Self {
            format: STORE_FORMAT,
            longest,
            recorded_at: unix_seconds(SystemTime::now()),
            created_by: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Read a record, ignoring one this envoy cannot interpret.
pub(crate) fn read(path: &Path) -> Option<Refusal> {
    let text = fs::read_to_string(path).ok()?;
    let refusal: Refusal = serde_json::from_str(&text).ok()?;
    (refusal.format == STORE_FORMAT).then_some(refusal)
}

pub(crate) fn write(path: &Path, refusal: &Refusal) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(refusal).expect("a refusal always serializes");
    fs::write(path, text)
}

/// Delete records nobody has consulted for `max_idle`.
///
/// A record is touched whenever it is used, so one that is still needed never
/// ages out.
pub(crate) fn forget_idle(directory: &Path, now: SystemTime, max_idle: Duration) {
    let Ok(records) = fs::read_dir(directory) else {
        return;
    };
    for record in records.flatten() {
        let idle = record
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_idle);
        if idle {
            let _ = fs::remove_file(record.path());
        }
    }
}
