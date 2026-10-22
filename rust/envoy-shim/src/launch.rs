//! Handing over to the envoy app.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use crate::error::EXIT_SOFTWARE;

/// Names the exact executable that is running, so a nested `envoy` call stays
/// on the same version as the one that started it.
pub const BIN_LOCK_VAR: &str = "ENVOY_BIN_LOCK";

/// Names the launcher itself, so something inside the app can start it again.
pub const BIN_FLOAT_VAR: &str = "ENVOY_BIN_FLOAT";

/// Run `exe` with `args` exactly as given, and return its exit code.
///
/// `float` is recorded for the app unless the environment already names a
/// launcher, so a nested call keeps pointing at the outermost one.
pub fn launch(exe: &Path, args: &[OsString], float: Option<&Path>) -> i32 {
    let mut command = Command::new(exe);
    command.args(args).env(BIN_LOCK_VAR, exe);
    if let Some(float) = float {
        if std::env::var_os(BIN_FLOAT_VAR).is_none_or(|existing| existing.is_empty()) {
            command.env(BIN_FLOAT_VAR, float);
        }
    }
    hand_over(command, exe)
}

/// On Unix the shim becomes the app, so signals, the terminal and the exit
/// code are the app's own and the shim costs nothing while it runs.
#[cfg(unix)]
fn hand_over(mut command: Command, exe: &Path) -> i32 {
    use std::os::unix::process::CommandExt;

    let error = command.exec();
    eprintln!("envoy: cannot run {}: {error}", exe.display());
    EXIT_SOFTWARE
}

/// Windows cannot replace a process, so the shim waits for the app and passes
/// its exit code on. Ctrl+C reaches both on a shared console; the shim ignores
/// it so the app decides what to do and its exit code is not lost.
#[cfg(not(unix))]
fn hand_over(mut command: Command, exe: &Path) -> i32 {
    let _ = ctrlc::set_handler(|| {});

    match command.status() {
        Ok(status) => status.code().unwrap_or(EXIT_SOFTWARE),
        Err(error) => {
            eprintln!("envoy: cannot run {}: {error}", exe.display());
            EXIT_SOFTWARE
        }
    }
}
