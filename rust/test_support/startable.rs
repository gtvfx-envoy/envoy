//! Starting a program that was only just written.
//!
//! Shared by the integration tests that copy a built executable and run the
//! copy, each of which includes this file as a module.

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

/// Times to try starting the program before giving up, a short while apart.
const ATTEMPTS: u32 = 100;
const PAUSE: Duration = Duration::from_millis(20);

/// Copy `from` to `to`, and wait until the copy can be started.
///
/// `probe` is an argument the program answers without side effects, such as
/// `--version`.
pub fn copy_startable(from: &Path, to: &Path, probe: &str) {
    fs::copy(from, to).expect("the executable is copied");
    wait_until_startable(to, probe);
}

/// Wait until `program` can be started.
///
/// Starting a file fails with "Text file busy" while any process has it open
/// for writing. Tests run on several threads, and a process that one thread
/// forks while another writes the copy holds the copy's handle until it starts
/// its own program, so a copy can stay busy for a moment after it is finished.
/// A file that starts once keeps starting: nothing can hold it open for writing
/// after that.
pub fn wait_until_startable(program: &Path, probe: &str) {
    for _ in 0..ATTEMPTS {
        match Command::new(program).arg(probe).output() {
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy => {
                thread::sleep(PAUSE);
            }
            _ => return,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs::OpenOptions;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    use super::*;

    #[test]
    fn a_program_still_open_for_writing_is_waited_for() {
        let program = std::env::temp_dir().join(format!("envoy_startable_{}", std::process::id()));
        fs::write(&program, "#!/bin/sh\nexit 0\n").expect("the program is written");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755))
            .expect("the program is executable");

        // Held open for writing for a while, as a forked child holds it.
        let writer = OpenOptions::new()
            .write(true)
            .open(&program)
            .expect("the program is opened");
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            drop(writer);
        });
        let refused = Command::new(&program)
            .output()
            .expect_err("a program open for writing cannot be started");
        assert_eq!(refused.kind(), io::ErrorKind::ExecutableFileBusy);

        let started = Instant::now();
        wait_until_startable(&program, "--version");
        let waited = started.elapsed();
        release.join().expect("the writer is released");

        assert!(
            waited >= Duration::from_millis(100),
            "it came back after {waited:?}, before the program could be started"
        );
        let run = Command::new(&program).output().expect("the program starts");
        assert!(run.status.success());
        fs::remove_file(&program).expect("the program is removed");
    }
}
