//! Copying a source bundle into a staging directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use rayon::prelude::*;

use crate::error::{io_error, Result, StoreError};
use crate::key::EntryKey;
use crate::observer::Observer;
use crate::state::STATE_DIR;

/// Worker threads copying files at once. A network share is latency-bound, so
/// several files in flight beat one fast stream.
const COPY_THREADS: usize = 8;

/// Attempts for one file, so a transient network hiccup does not fail a fill.
const COPY_ATTEMPTS: u32 = 3;

/// A directory this deep is far more likely a link loop than a real bundle.
const MAX_DEPTH: usize = 64;

/// How often progress is reported.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Everything in a source bundle, found before anything is copied.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub(crate) directories: Vec<PathBuf>,
    pub(crate) files: Vec<PlannedFile>,
    pub(crate) links: Vec<PlannedLink>,
    pub(crate) bytes: u64,
    /// Length of the longest path relative to the bundle root.
    pub(crate) longest: usize,
}

#[derive(Debug)]
pub(crate) struct PlannedFile {
    pub(crate) relative: PathBuf,
    pub(crate) size: u64,
}

#[derive(Debug)]
pub(crate) struct PlannedLink {
    pub(crate) relative: PathBuf,
    pub(crate) target: PathBuf,
}

/// Walk `source` and list what a copy would need to create.
///
/// The store's own `.store` directory is never copied. Symbolic links are
/// recreated as links on Unix; on Windows a link is followed and copied as
/// what it points to.
pub(crate) fn plan(source: &Path) -> io::Result<Plan> {
    let mut plan = Plan::default();
    let mut pending = vec![(source.to_path_buf(), PathBuf::new(), 0_usize)];

    while let Some((directory, relative, depth)) = pending.pop() {
        if depth > MAX_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("directory tree is too deep: {}", directory.display()),
            ));
        }

        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if depth == 0 && name == STATE_DIR {
                continue;
            }

            let child = relative.join(&name);
            plan.longest = plan.longest.max(child.as_os_str().len());

            let file_type = entry.file_type()?;
            let (is_directory, size) = if file_type.is_symlink() {
                if cfg!(unix) {
                    plan.links.push(PlannedLink {
                        target: fs::read_link(entry.path())?,
                        relative: child,
                    });
                    continue;
                }
                let metadata = fs::metadata(entry.path())?;
                (metadata.is_dir(), metadata.len())
            } else if file_type.is_dir() {
                (true, 0)
            } else {
                (false, entry.metadata()?.len())
            };

            if is_directory {
                plan.directories.push(child.clone());
                pending.push((entry.path(), child, depth + 1));
            } else {
                plan.bytes += size;
                plan.files.push(PlannedFile {
                    relative: child,
                    size,
                });
            }
        }
    }

    Ok(plan)
}

/// Create the planned directories, links, and files under `staging`.
pub(crate) fn copy_plan(
    source: &Path,
    staging: &Path,
    plan: &Plan,
    key: &EntryKey,
    observer: &dyn Observer,
) -> Result<()> {
    fs::create_dir_all(staging).map_err(io_error(staging))?;
    for directory in &plan.directories {
        let path = staging.join(directory);
        fs::create_dir_all(&path).map_err(io_error(&path))?;
    }
    for link in &plan.links {
        create_link(&link.target, &staging.join(&link.relative))?;
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(COPY_THREADS)
        .build()
        .map_err(|error| StoreError::Io {
            path: staging.to_path_buf(),
            source: io::Error::other(error),
        })?;

    let copied = AtomicU64::new(0);
    let finished = AtomicBool::new(false);

    thread::scope(|scope| {
        let reporter = scope.spawn(|| {
            while !finished.load(Ordering::Acquire) {
                thread::park_timeout(PROGRESS_INTERVAL);
                if !finished.load(Ordering::Acquire) {
                    observer.progress(key, copied.load(Ordering::Relaxed), plan.bytes);
                }
            }
        });

        let outcome = pool.install(|| {
            plan.files.par_iter().try_for_each(|file| {
                copy_file(source, staging, file)?;
                copied.fetch_add(file.size, Ordering::Relaxed);
                Ok(())
            })
        });

        finished.store(true, Ordering::Release);
        reporter.thread().unpark();
        outcome
    })
}

fn copy_file(source: &Path, staging: &Path, file: &PlannedFile) -> Result<()> {
    let from = source.join(&file.relative);
    let to = staging.join(&file.relative);

    let mut attempt = 0;
    loop {
        match fs::copy(&from, &to) {
            Ok(_) => {
                keep_modified_time(&from, &to);
                return Ok(());
            }
            Err(error) if attempt + 1 < COPY_ATTEMPTS && is_transient(&error) => {
                attempt += 1;
                thread::sleep(Duration::from_millis(100 * u64::from(attempt)));
            }
            Err(error) => return Err(io_error(from)(error)),
        }
    }
}

/// Give the copy the original's modified time.
///
/// Windows copies it already. Elsewhere `fs::copy` keeps permissions only, and
/// compiled Python is judged stale when its source's time differs from the one
/// it recorded, so a copy with a new time would be recompiled on every machine.
#[cfg(unix)]
fn keep_modified_time(from: &Path, to: &Path) {
    if let Ok(modified) = fs::metadata(from).and_then(|metadata| metadata.modified()) {
        // A read-only handle is enough: the copy is ours, and so is its time.
        if let Ok(copy) = fs::File::open(to) {
            let _ = copy.set_modified(modified);
        }
    }
}

#[cfg(not(unix))]
fn keep_modified_time(_from: &Path, _to: &Path) {}

#[cfg(unix)]
fn create_link(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).map_err(io_error(link))
}

#[cfg(not(unix))]
fn create_link(_target: &Path, link: &Path) -> Result<()> {
    Err(StoreError::Io {
        path: link.to_path_buf(),
        source: io::Error::other("links are only recreated on Unix"),
    })
}

/// Count the files and bytes under `directory`, to compare with the plan.
///
/// The store's own `.store` directory at the top is left out, so a staged copy
/// and a finished entry measure the same.
pub(crate) fn measure(directory: &Path) -> io::Result<(u64, u64)> {
    let mut files = 0;
    let mut bytes = 0;
    let mut pending = vec![directory.to_path_buf()];

    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            if current == directory && entry.file_name() == STATE_DIR {
                continue;
            }

            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_symlink() {
                continue;
            } else {
                files += 1;
                bytes += entry.metadata()?.len();
            }
        }
    }

    Ok((files, bytes))
}

/// Rename `from` to `to`, retrying while antivirus or an indexer holds a
/// handle inside the directory.
pub(crate) fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    const ATTEMPTS: u32 = 12;

    let mut attempt = 0;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) if attempt + 1 < ATTEMPTS && is_busy(&error) => {
                attempt += 1;
                thread::sleep(Duration::from_millis(50 * u64::from(attempt.min(8))));
            }
            Err(error) => return Err(error),
        }
    }
}

/// `true` when a file operation failed for a reason a retry can cure.
fn is_transient(error: &io::Error) -> bool {
    use io::ErrorKind::{
        ConnectionAborted, ConnectionReset, Interrupted, NetworkDown, NetworkUnreachable, TimedOut,
        WouldBlock,
    };

    matches!(
        error.kind(),
        TimedOut
            | Interrupted
            | WouldBlock
            | ConnectionReset
            | ConnectionAborted
            | NetworkDown
            | NetworkUnreachable
    ) || is_busy(error)
        || is_network_glitch(error)
}

/// `true` when another process has the file or directory open (Windows).
fn is_busy(error: &io::Error) -> bool {
    // ACCESS_DENIED, SHARING_VIOLATION, LOCK_VIOLATION
    cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

/// `true` for the Windows errors a flaky SMB connection reports.
fn is_network_glitch(error: &io::Error) -> bool {
    // BAD_NETPATH, NETWORK_BUSY, UNEXP_NET_ERR, NETNAME_DELETED, SEM_TIMEOUT
    cfg!(windows) && matches!(error.raw_os_error(), Some(53 | 54 | 59 | 64 | 121))
}

/// Remove a directory tree, giving up silently: a leftover is retried by the
/// next cleanup.
pub(crate) fn remove_tree(path: &Path) {
    if let Err(error) = fs::remove_dir_all(path) {
        tracing::debug!(path = %path.display(), "Could not remove {error}");
    }
}
