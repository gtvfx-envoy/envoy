use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs4::{FileExt, TryLockError};

use crate::error::{io_error, Result};

/// An advisory lock file guarding one cached entry.
///
/// Readers hold it shared, so any number of envoys can run from an entry at
/// once. A fill, prune, or clear holds it exclusive. The operating system
/// releases the lock when the holder exits, so a crashed envoy never leaves a
/// stale lock behind.
pub(crate) struct LockFile {
    file: File,
    path: PathBuf,
}

impl LockFile {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_error(parent))?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io_error(path))?;

        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Take the lock shared without waiting. `false` means a writer holds it.
    pub(crate) fn try_shared(&self) -> Result<bool> {
        match FileExt::try_lock_shared(&self.file) {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(source)) => Err(io_error(&self.path)(source)),
        }
    }

    /// Take the lock exclusive without waiting. `false` means it is in use.
    pub(crate) fn try_exclusive(&self) -> Result<bool> {
        match FileExt::try_lock(&self.file) {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(source)) => Err(io_error(&self.path)(source)),
        }
    }

    /// Release a lock this handle holds.
    pub(crate) fn unlock(&self) -> Result<()> {
        FileExt::unlock(&self.file).map_err(io_error(&self.path))
    }

    /// Keep the shared lock this handle holds for as long as the lease lives.
    pub(crate) fn into_lease(self) -> Lease {
        Lease { _file: self.file }
    }
}

/// A shared hold on a cached entry.
///
/// While a lease exists, prune and clear leave the entry in place. Dropping it
/// releases the hold, and so does the process exiting.
#[derive(Debug)]
pub struct Lease {
    _file: File,
}
