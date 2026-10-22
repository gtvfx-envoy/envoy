use std::time::Duration;

use crate::key::EntryKey;

/// Receives what the store is doing, so a caller can show progress.
///
/// Every method has an empty default, so an implementor overrides only what it
/// wants to show. Methods can be called from a worker thread.
pub trait Observer: Sync {
    /// Another envoy is filling or removing this entry; this one is waiting.
    fn waiting(&self, _key: &EntryKey) {}

    /// A fill is starting.
    fn started(&self, _key: &EntryKey, _files: u64, _bytes: u64) {}

    /// Bytes copied so far out of the total.
    fn progress(&self, _key: &EntryKey, _done: u64, _total: u64) {}

    /// A fill ended, whether or not it succeeded.
    fn finished(&self, _key: &EntryKey, _elapsed: Duration) {}
}

/// An [`Observer`] that reports nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct Quiet;

impl Observer for Quiet {}
