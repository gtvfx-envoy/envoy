use std::time::Duration;

use crate::key::EntryKey;

/// Receives what the store is doing, so a caller can show progress.
///
/// Every method has an empty default, so an implementor overrides only what it
/// wants to show. Methods can be called from a worker thread, and from several
/// at once when bundles are copied together.
pub trait Observer: Sync {
    /// Another envoy is filling or removing this entry; this one is waiting.
    fn waiting(&self, _key: &EntryKey) {}

    /// A fill is starting.
    fn started(&self, _key: &EntryKey, _files: u64, _bytes: u64) {}

    /// Bytes copied so far out of the total.
    fn progress(&self, _key: &EntryKey, _done: u64, _total: u64) {}

    /// A fill ended, whether or not it succeeded.
    fn finished(&self, _key: &EntryKey, _elapsed: Duration) {}

    /// A fill succeeded and the entry is ready: `bytes` were copied in `elapsed`.
    fn cached(&self, _key: &EntryKey, _bytes: u64, _elapsed: Duration) {}

    /// Something went wrong that does not stop the run, such as a bundle that
    /// could not be copied and so runs from where it is.
    ///
    /// Return `true` when the message was shown. The caller logs it otherwise.
    /// An observer that draws on the screen shows it itself, so the message
    /// cannot land inside what it has drawn while other copies are running.
    fn warning(&self, _message: &str) -> bool {
        false
    }
}

/// An [`Observer`] that reports nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct Quiet;

impl Observer for Quiet {}
