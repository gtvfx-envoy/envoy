//! Copying a source bundle into a staging directory.

use std::fs;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::thread;
use std::time::Duration;

use crate::error::{io_error, Result, StoreError};
use crate::key::EntryKey;
use crate::observer::Observer;
use crate::settings;
use crate::state::STATE_DIR;

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

#[derive(Clone, Debug)]
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

    let pool = copy_pool(staging)?;

    let copied = Arc::new(AtomicU64::new(0));
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

        let outcome = copy_files(pool, source, staging, &plan.files, &copied);

        finished.store(true, Ordering::Release);
        reporter.thread().unpark();
        outcome
    })
}

/// Copy `files` from `source` into `staging`, one pool job for each file.
fn copy_files(
    pool: &rayon::ThreadPool,
    source: &Path,
    staging: &Path,
    files: &[PlannedFile],
    copied: &Arc<AtomicU64>,
) -> Result<()> {
    let copy = Arc::new(FileCopy {
        source: source.to_path_buf(),
        staging: staging.to_path_buf(),
        files: spread(files, pool.current_num_threads()),
        copied: Arc::clone(copied),
    });
    let count = copy.files.len();
    let for_job = Arc::clone(&copy);

    run_jobs(
        pool,
        count,
        move |index| for_job.copy(index),
        move |index| copy.stopped(index),
    )
}

/// `files` in the order the jobs should take them: one from each of `lanes`
/// equal stretches of the list in turn.
///
/// The list is in directory order, so handing the jobs out as listed puts every
/// thread in the same directory at once. On a cold fill of a real stack that
/// measured a few percent slower than spreading the threads over the list,
/// which is what a parallel iterator does.
fn spread(files: &[PlannedFile], lanes: usize) -> Vec<PlannedFile> {
    let lane_length = files.len().div_ceil(lanes.max(1)).max(1);
    let mut spread = Vec::with_capacity(files.len());
    for position in 0..lane_length {
        for start in (0..files.len()).step_by(lane_length) {
            if let Some(file) = files.get(start + position) {
                spread.push(file.clone());
            }
        }
    }
    spread
}

/// What the jobs of one copy share.
struct FileCopy {
    source: PathBuf,
    staging: PathBuf,
    files: Vec<PlannedFile>,
    copied: Arc<AtomicU64>,
}

impl FileCopy {
    fn copy(&self, index: usize) -> Result<()> {
        let file = &self.files[index];
        copy_file(&self.source, &self.staging, file)?;
        self.copied.fetch_add(file.size, Ordering::Relaxed);
        Ok(())
    }

    /// The error for a job that panicked instead of finishing.
    fn stopped(&self, index: usize) -> StoreError {
        StoreError::Io {
            path: self.source.join(&self.files[index].relative),
            source: io::Error::other("the copy stopped unexpectedly"),
        }
    }
}

/// What the jobs of one [`run_jobs`] call share.
struct Jobs<Job, Stopped> {
    job: Job,
    stopped: Stopped,
    failed: AtomicBool,
}

/// Run `count` jobs on `pool`, one pool job each, and wait for all of them.
///
/// The calling thread hands the jobs over and then waits here, outside the
/// pool, so a call ends when its own jobs have. A parallel iterator run inside
/// the pool does not give that: a worker waiting for the last pieces of one
/// call takes on another call's whole job meanwhile, and the first cannot end
/// before the second does. Jobs start in the order they were handed over, other
/// callers' included, so the pool is never idle while any call has work.
///
/// The first failure stops the jobs that have not started. It is returned once
/// every job that did start has ended, so the caller may remove what they wrote.
/// A job that panics counts as a failure, reported as `stopped(index)`.
fn run_jobs<E: Send + 'static>(
    pool: &rayon::ThreadPool,
    count: usize,
    job: impl Fn(usize) -> std::result::Result<(), E> + Send + Sync + 'static,
    stopped: impl Fn(usize) -> E + Send + Sync + 'static,
) -> std::result::Result<(), E> {
    let jobs = Arc::new(Jobs {
        job,
        stopped,
        failed: AtomicBool::new(false),
    });
    let (ended, results) = mpsc::channel();

    for index in 0..count {
        let jobs = Arc::clone(&jobs);
        let ended = ended.clone();
        pool.spawn_fifo(move || {
            let result = if jobs.failed.load(Ordering::Relaxed) {
                Ok(())
            } else {
                catch_unwind(AssertUnwindSafe(|| (jobs.job)(index)))
                    .unwrap_or_else(|_| Err((jobs.stopped)(index)))
            };
            // The receiver reads until every sender is gone, so this cannot fail.
            let _ = ended.send(result);
        });
    }
    drop(ended);

    let mut failure = None;
    for result in results {
        if let Err(error) = result {
            jobs.failed.store(true, Ordering::Relaxed);
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

/// The threads that copy files, started with the first fill and kept for the
/// rest of the process.
///
/// Every fill shares them. A network share is latency-bound, so several files in
/// flight beat one fast stream, and sharing keeps that number the same however
/// many bundles are being copied together. `ENVOY_CACHE_THREADS` sets it.
fn copy_pool(staging: &Path) -> Result<&'static rayon::ThreadPool> {
    static POOL: OnceLock<std::result::Result<rayon::ThreadPool, String>> = OnceLock::new();

    POOL.get_or_init(|| {
        let threads =
            settings::copy_threads(std::env::var(settings::CACHE_THREADS_VAR).ok().as_deref());
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("envoy-copy-{index}"))
            .build()
            .map_err(|error| error.to_string())
    })
    .as_ref()
    .map_err(|message| StoreError::Io {
        path: staging.to_path_buf(),
        source: io::Error::other(message.clone()),
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use super::{run_jobs, spread, PlannedFile};

    const LIMIT: Duration = Duration::from_secs(10);

    fn pool(threads: usize) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("pool builds")
    }

    fn listed(count: usize) -> Vec<PlannedFile> {
        (0..count)
            .map(|index| PlannedFile {
                relative: PathBuf::from(index.to_string()),
                size: 0,
            })
            .collect()
    }

    fn names(files: &[PlannedFile]) -> Vec<String> {
        files
            .iter()
            .map(|file| file.relative.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn spreading_takes_one_file_from_each_stretch_in_turn() {
        let spread = spread(&listed(10), 3);

        assert_eq!(
            names(&spread),
            ["0", "4", "8", "1", "5", "9", "2", "6", "3", "7"]
        );
    }

    #[test]
    fn spreading_keeps_every_file_exactly_once() {
        for (count, lanes) in [(0, 4), (1, 16), (5, 16), (100, 1), (101, 16), (4096, 7)] {
            let files = listed(count);

            let mut kept = names(&spread(&files, lanes));
            kept.sort();
            let mut original = names(&files);
            original.sort();

            assert_eq!(kept, original, "{count} files in {lanes} lanes");
        }
    }

    /// Something a job can wait on until the test lets it go.
    struct Gate {
        opener: mpsc::Sender<()>,
        waiter: Mutex<mpsc::Receiver<()>>,
    }

    impl Gate {
        fn new() -> Arc<Gate> {
            let (opener, waiter) = mpsc::channel();
            Arc::new(Gate {
                opener,
                waiter: Mutex::new(waiter),
            })
        }

        fn wait(&self) {
            self.waiter
                .lock()
                .expect("gate is not poisoned")
                .recv_timeout(LIMIT)
                .expect("the test opens the gate");
        }

        fn open(&self) {
            self.opener.send(()).expect("the gate is still there");
        }
    }

    #[test]
    fn jobs_start_in_the_order_they_were_handed_over() {
        let pool = pool(1);
        let order = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&order);

        let result: Result<(), String> = run_jobs(
            &pool,
            20,
            move |index| {
                seen.lock().expect("not poisoned").push(index);
                Ok(())
            },
            |index| format!("job {index} stopped"),
        );

        assert_eq!(result, Ok(()));
        assert_eq!(
            *order.lock().expect("not poisoned"),
            (0..20).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_call_ends_when_its_own_jobs_do_whatever_another_call_is_doing() {
        let pool = Arc::new(pool(3));
        let (first_waits, first_waiting) = mpsc::channel();
        let (second_waits, second_waiting) = mpsc::channel();
        let (first_ended, first_end) = mpsc::channel();
        let (second_ended, second_end) = mpsc::channel();
        let first_gate = Gate::new();
        let second_gate = Gate::new();

        // The first call: one quick job, and one that waits to be let go.
        let first = {
            let (pool, gate) = (Arc::clone(&pool), Arc::clone(&first_gate));
            thread::spawn(move || {
                let result: Result<(), String> = run_jobs(
                    &pool,
                    2,
                    move |index| {
                        if index == 1 {
                            first_waits.send(()).expect("test is listening");
                            gate.wait();
                        }
                        Ok(())
                    },
                    |index| format!("job {index} stopped"),
                );
                first_ended.send(result).expect("test is listening");
            })
        };
        first_waiting
            .recv_timeout(LIMIT)
            .expect("the first call's second job started");

        // The second call starts while the first still waits, and its job waits too.
        let second = {
            let (pool, gate) = (Arc::clone(&pool), Arc::clone(&second_gate));
            thread::spawn(move || {
                let result: Result<(), String> = run_jobs(
                    &pool,
                    1,
                    move |_| {
                        second_waits.send(()).expect("test is listening");
                        gate.wait();
                        Ok(())
                    },
                    |index| format!("job {index} stopped"),
                );
                second_ended.send(result).expect("test is listening");
            })
        };
        second_waiting
            .recv_timeout(LIMIT)
            .expect("the second call's job started");

        // Letting the first go ends the first call, though the second is still busy.
        first_gate.open();
        let result = first_end
            .recv_timeout(LIMIT)
            .expect("the first call ended without waiting for the second");
        assert_eq!(result, Ok(()));
        assert!(
            second_end.try_recv().is_err(),
            "the second call is still waiting"
        );

        second_gate.open();
        assert_eq!(second_end.recv_timeout(LIMIT).expect("second ends"), Ok(()));
        first.join().expect("first thread ends");
        second.join().expect("second thread ends");
    }

    #[test]
    fn the_first_failure_is_returned_once_every_job_that_started_has_ended() {
        let pool = pool(4);
        let running = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let (job_running, job_started) = (Arc::clone(&running), Arc::clone(&started));

        let result: Result<(), String> = run_jobs(
            &pool,
            2000,
            move |index| {
                job_running.fetch_add(1, Ordering::SeqCst);
                job_started.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(2));
                job_running.fetch_sub(1, Ordering::SeqCst);
                if index == 5 {
                    Err("job 5 failed".to_string())
                } else {
                    Ok(())
                }
            },
            |index| format!("job {index} stopped"),
        );

        assert_eq!(result, Err("job 5 failed".to_string()));
        assert_eq!(running.load(Ordering::SeqCst), 0, "a job is still running");
        assert!(
            started.load(Ordering::SeqCst) < 1000,
            "the jobs that had not started were not skipped"
        );
    }

    #[test]
    fn a_job_that_panics_is_a_failure_and_the_pool_carries_on() {
        let pool = pool(2);

        let result: Result<(), String> = run_jobs(
            &pool,
            5,
            |index| {
                if index == 2 {
                    panic!("job 2 panicked on purpose");
                }
                Ok(())
            },
            |index| format!("job {index} stopped"),
        );
        assert_eq!(result, Err("job 2 stopped".to_string()));

        let again: Result<(), String> =
            run_jobs(&pool, 3, |_| Ok(()), |index| format!("job {index} stopped"));
        assert_eq!(again, Ok(()));
    }
}
