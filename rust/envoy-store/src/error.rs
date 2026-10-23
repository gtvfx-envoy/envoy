use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Errors raised by the store.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A namespace, name, or version cannot be used as a store path.
    #[error("invalid {part} '{value}' for a store entry: {reason}")]
    InvalidKey {
        part: &'static str,
        value: String,
        reason: &'static str,
    },

    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The store was written by a newer envoy that uses a layout this one
    /// does not understand.
    #[error(
        "the store at {path} uses format {found}, but this envoy understands format {supported}"
    )]
    UnsupportedFormat {
        path: PathBuf,
        found: u32,
        supported: u32,
    },

    /// The source is not a directory.
    #[error("the source of {key} is not a directory: {path}")]
    InvalidSource { key: String, path: PathBuf },

    /// The longest path in the entry would not fit under the store root.
    ///
    /// The refusal is remembered, so the source is not walked again until the
    /// store moves or the rule changes.
    #[error(
        "{key} contains paths that would reach {needed} characters under {root}; \
the limit is {limit}"
    )]
    PathTooLong {
        key: String,
        needed: usize,
        limit: usize,
        root: PathBuf,
        /// `true` when an earlier run already found this and was reported.
        remembered: bool,
    },

    /// The volume holding the store has too little free space.
    #[error(
        "not enough free space for {key} in {path}: needs {needed} bytes, {available} available"
    )]
    InsufficientSpace {
        key: String,
        path: PathBuf,
        needed: u64,
        available: u64,
    },

    /// What was copied does not match what was planned.
    #[error("{key} was not copied completely: expected {expected_files} files and {expected_bytes} bytes, found {found_files} files and {found_bytes} bytes")]
    Incomplete {
        key: String,
        expected_files: u64,
        expected_bytes: u64,
        found_files: u64,
        found_bytes: u64,
    },
}

/// Result alias for store operations.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Build a `map_err` closure that attaches `path` to an I/O error.
pub(crate) fn io_error(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> StoreError {
    let path = path.into();
    move |source| StoreError::Io { path, source }
}
