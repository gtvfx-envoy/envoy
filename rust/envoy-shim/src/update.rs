//! Keeping the launcher itself up to date.
//!
//! The share holds a pointer, `shim/current.json`, that names the launcher
//! version the team runs, and the launcher for each platform in
//! `shim/<version>/<target>/`. About once a day the launcher compares its own
//! version with the pointer. When they differ it copies the published launcher
//! beside itself, checks that the copy runs and reports the version the pointer
//! names, and swaps it in. A running program can be renamed on every platform,
//! so the swap needs no restart: the launch after this one is the new launcher.
//!
//! Only a launcher installed in `<config root>/bin` updates itself, so a build
//! run from a checkout is never replaced. `ENVOY_SHIM_UPDATE=off` turns it off.
//! Everything is best effort: a launch never fails, or waits long, because an
//! update could not be made.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use envoy_config::locate::CONFIG_ROOT_VAR;
use envoy_config::redirect::is_safe_version;
use envoy_store::settings::{self, parse_switch};
use serde::Deserialize;

use crate::error::{EXIT_CONFIG, EXIT_UNAVAILABLE};
use crate::source::{read_with_timeout, within, DEFAULT_TIMEOUT};
use crate::version::same_version;
use crate::{how_long, VERSION};

/// `off`, `false`, `no` or `0` stops the launcher updating itself.
pub const SWITCH_VAR: &str = "ENVOY_SHIM_UPDATE";

/// The platform this launcher was built for, as the share's `shim/` folders name it.
pub const TARGET: &str = env!("ENVOY_SHIM_TARGET");

/// How long to leave between looks at the share.
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Records when the share was last looked at, in `<config root>/shim`.
const STAMP: &str = "last-check";

const POINTER: &str = "current.json";

/// The newest pointer layout this launcher understands.
const SCHEMA: u32 = 1;

/// How long copying the new launcher, or trying it out, may take before giving up.
const WORK_TIMEOUT: Duration = Duration::from_secs(10);

/// A lock this old was left by a launch that was killed part way through.
const STALE_LOCK: Duration = Duration::from_secs(10 * 60);

/// Held, in `<config root>/shim`, while the launcher is updated or tidied.
const LOCK: &str = "update.lock";

const STAGING: &str = ".update";

#[derive(Deserialize)]
struct Pointer {
    schema: Option<u32>,
    version: String,
}

/// What an attempt to update came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// This launcher is the one the pointer names.
    Current(String),
    /// The launcher in the install folder was replaced; the next launch is the new one.
    Updated { from: String, to: String },
}

/// The file name of a program on this platform.
pub fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn switched_off(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env(SWITCH_VAR).and_then(|value| parse_switch(&value)) == Some(false)
}

fn config_root() -> PathBuf {
    settings::config_root(&|name| std::env::var_os(name))
}

/// The folder the running launcher was installed in, when it is where the
/// installer puts it.
fn installed_dir(config_root: &Path) -> Option<PathBuf> {
    installed_in(&std::env::current_exe().ok()?, config_root)
}

fn installed_in(exe: &Path, config_root: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    match (
        fs::canonicalize(dir),
        fs::canonicalize(config_root.join("bin")),
    ) {
        (Ok(here), Ok(expected)) if here == expected => Some(dir.to_path_buf()),
        _ => None,
    }
}

/// Look for a newer launcher if this one is installed, updating is on, and none
/// was looked for in the last day. Also clears away what an earlier update left.
pub fn maybe_update(env: &dyn Fn(&str) -> Option<String>) {
    if switched_off(env) {
        return;
    }
    let root = config_root();
    let Some(dir) = installed_dir(&root) else {
        return;
    };
    let state = root.join("shim");
    let stamp = state.join(STAMP);
    let share = env(CONFIG_ROOT_VAR).filter(|value| !value.trim().is_empty());

    let due = share.is_some() && !modified_within(&stamp, INTERVAL);
    if !due && !has_leftovers(&dir) {
        return;
    }

    // One launch at a time: a launch that finds the lock taken leaves the work
    // to the one that holds it.
    let Some(_lock) = Lock::try_acquire(&state) else {
        return;
    };
    cleanup(&dir);

    // Looked at again, now that no other launch can be about to do the same.
    let Some(share) = share.filter(|_| !modified_within(&stamp, INTERVAL)) else {
        return;
    };

    // Recorded before trying, so a failure is not repeated by every launch.
    if fs::write(&stamp, b"").is_err() {
        return;
    }
    if let Ok(Outcome::Updated { to, .. }) = update(Path::new(&share), &dir) {
        eprintln!("envoy: the launcher was updated to {to}; it takes over at the next run");
    }
}

/// `--envoy-shim-update`: look now, whatever the switch and the time since the
/// last look, and say what happened.
pub fn run_now(env: &dyn Fn(&str) -> Option<String>) -> i32 {
    let root = config_root();
    let Some(dir) = installed_dir(&root) else {
        println!(
            "This launcher is not installed in {}, so it does not update itself.",
            root.join("bin").display()
        );
        return 0;
    };
    let Some(share) = env(CONFIG_ROOT_VAR).filter(|value| !value.trim().is_empty()) else {
        eprintln!(
            "envoy: {CONFIG_ROOT_VAR} is not set, so there is no share to look for a newer launcher on"
        );
        return EXIT_CONFIG;
    };

    let state = root.join("shim");
    let Some(_lock) = Lock::try_acquire(&state) else {
        println!("Another launch is updating the launcher right now; try again in a moment.");
        return 0;
    };
    cleanup(&dir);
    let _ = fs::write(state.join(STAMP), b"");
    match update(Path::new(&share), &dir) {
        Ok(Outcome::Current(version)) => println!("The launcher is current ({version})."),
        Ok(Outcome::Updated { from, to }) => {
            println!("Updated the launcher from {from} to {to}; it takes over at the next run.");
        }
        Err(reason) => {
            eprintln!("envoy: {reason}");
            return EXIT_UNAVAILABLE;
        }
    }
    0
}

/// One line for `--envoy-shim-resolve`: the launcher's own version and whether it updates itself.
pub fn status(env: &dyn Fn(&str) -> Option<String>) -> String {
    let root = config_root();
    let Some(dir) = installed_dir(&root) else {
        return format!(
            "envoy-shim {VERSION} (not installed in {}, so it does not update itself)",
            root.join("bin").display()
        );
    };
    let how = if switched_off(env) {
        format!("updates are off, set by {SWITCH_VAR}")
    } else {
        match fs::metadata(root.join("shim").join(STAMP)).and_then(|meta| meta.modified()) {
            Ok(when) => format!(
                "looked for an update {} ago",
                how_long(SystemTime::now().duration_since(when).unwrap_or_default())
            ),
            Err(_) => "has not looked for an update yet".to_string(),
        }
    };
    format!(
        "envoy-shim {VERSION} (installed in {}; {how})",
        dir.display()
    )
}

/// Compare with the pointer on the share, and replace the installed launcher
/// when the pointer names another version.
pub fn update(share: &Path, dir: &Path) -> Result<Outcome, String> {
    let folder = share.join("shim");
    let pointer_path = folder.join(POINTER);
    let text = read_with_timeout(&pointer_path, DEFAULT_TIMEOUT)
        .map_err(|error| format!("cannot read {}: {error}", pointer_path.display()))?;
    let pointer: Pointer = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|error| format!("{} is not valid: {error}", pointer_path.display()))?;

    if pointer.schema.is_some_and(|schema| schema > SCHEMA) {
        return Err(format!(
            "{} was written for a newer launcher; run the installer again to get it",
            pointer_path.display()
        ));
    }
    if !is_safe_version(&pointer.version) {
        return Err(format!(
            "{} names '{}', which is not a safe version name",
            pointer_path.display(),
            pointer.version
        ));
    }
    if same_version(&pointer.version, VERSION) {
        return Ok(Outcome::Current(VERSION.to_string()));
    }

    let published = folder
        .join(&pointer.version)
        .join(TARGET)
        .join(exe_name("envoy"));
    let staging = dir.join(STAGING);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir(&staging)
        .map_err(|error| format!("cannot create {}: {error}", staging.display()))?;
    let swapped = stage(&published, &pointer.version, &staging).and_then(|staged| {
        swap(dir, &staged).map_err(|error| format!("cannot install it: {error}"))
    });
    let _ = fs::remove_dir_all(&staging);
    swapped?;

    Ok(Outcome::Updated {
        from: VERSION.to_string(),
        to: pointer.version,
    })
}

/// Copy the published launcher into `staging` and check that it runs and is the
/// version the pointer promised.
fn stage(published: &Path, version: &str, staging: &Path) -> Result<PathBuf, String> {
    let staged = staging.join(exe_name("envoy"));

    let (from, to) = (published.to_path_buf(), staged.clone());
    within(WORK_TIMEOUT, move || fs::copy(&from, &to).map(|_| ()))
        .map_err(|error| format!("cannot copy {}: {error}", published.display()))?;
    make_executable(&staged).map_err(|error| format!("cannot make it runnable: {error}"))?;

    let trial = staged.clone();
    let output = within(WORK_TIMEOUT, move || {
        Command::new(&trial)
            .arg("--envoy-shim-version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
    })
    .map_err(|error| format!("the published launcher does not run: {error}"))?;

    let said = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let reported = said.strip_prefix("envoy-shim ").unwrap_or(&said);
    if !output.status.success() || !same_version(reported, version) {
        return Err(format!(
            "the launcher published as {version} reports '{said}', so it was not installed"
        ));
    }
    Ok(staged)
}

/// Put `staged` in place of the launcher under each of its names. On Unix `en`
/// is a link to `envoy` and follows it; on Windows it is a copy, replaced in turn.
fn swap(dir: &Path, staged: &Path) -> io::Result<()> {
    for stem in ["envoy", "en"] {
        let target = dir.join(exe_name(stem));
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => continue,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound && stem == "en" => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        replace(&target, staged)?;
    }
    Ok(())
}

/// Replace `target` with a copy of `staged`, also while `target` is running.
///
/// The copy is made beside `target` so that it is on the same volume and the
/// final step is a rename. Unix renames over the running program. Windows
/// cannot overwrite a running program but can rename it, so it moves the old
/// one aside first; the leftover is removed by a later launch.
fn replace(target: &Path, staged: &Path) -> io::Result<()> {
    let incoming = beside(target, ".new");
    fs::copy(staged, &incoming)?;
    if let Err(error) = make_executable(&incoming).and_then(|()| install(&incoming, target)) {
        let _ = fs::remove_file(&incoming);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn install(incoming: &Path, target: &Path) -> io::Result<()> {
    fs::rename(incoming, target)
}

#[cfg(not(unix))]
fn install(incoming: &Path, target: &Path) -> io::Result<()> {
    let previous = beside(target, ".old");
    let _ = fs::remove_file(&previous);
    let moved_aside = match fs::rename(target, &previous) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if let Err(error) = fs::rename(incoming, target) {
        if moved_aside {
            let _ = fs::rename(&previous, target);
        }
        return Err(error);
    }
    let _ = fs::remove_file(&previous);
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// What an earlier swap or a stopped update may have left in the install folder.
fn leftovers(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for stem in ["envoy", "en"] {
        for suffix in [".old", ".new"] {
            found.push(beside(&dir.join(exe_name(stem)), suffix));
        }
    }
    found.push(dir.join(STAGING));
    found
}

fn has_leftovers(dir: &Path) -> bool {
    leftovers(dir)
        .iter()
        .any(|path| fs::symlink_metadata(path).is_ok())
}

/// Remove what an earlier swap or a stopped update left in the install folder.
///
/// Only while holding the [`Lock`]: with it, nothing found here belongs to an
/// update still under way. (On Windows the launcher that swapped itself is
/// still running as `envoy.exe.old` and cannot be deleted yet; a later launch
/// finds it gone.)
fn cleanup(dir: &Path) {
    for path in leftovers(dir) {
        if path.file_name().is_some_and(|name| name == STAGING) {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Held while the launcher is updated or tidied, so that two launches starting
/// together do not both swap it, and one never removes what another is part
/// way through installing.
struct Lock(PathBuf);

impl Lock {
    /// Take the lock, or return `None` when another launch holds it.
    fn try_acquire(state: &Path) -> Option<Self> {
        fs::create_dir_all(state).ok()?;
        let path = state.join(LOCK);
        // The second try is for a lock cleared away because its holder was killed.
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Some(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if modified_within(&path, STALE_LOCK) {
                        return None;
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// `path` with `suffix` added to its file name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn modified_within(path: &Path, interval: Duration) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|when| SystemTime::now().duration_since(when).ok())
        .is_some_and(|age| age < interval)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn share_with_pointer(contents: &str) -> tempfile::TempDir {
        let share = tempdir().expect("tempdir is created");
        fs::create_dir_all(share.path().join("shim")).expect("shim folder is created");
        fs::write(share.path().join("shim").join(POINTER), contents).expect("pointer is written");
        share
    }

    #[test]
    fn only_the_folder_the_installer_uses_counts_as_installed() {
        let root = tempdir().expect("tempdir is created");
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).expect("bin is created");
        let elsewhere = root.path().join("target");
        fs::create_dir_all(&elsewhere).expect("target is created");

        assert_eq!(
            installed_in(&bin.join(exe_name("envoy")), root.path()),
            Some(bin.clone())
        );
        assert_eq!(
            installed_in(&elsewhere.join(exe_name("envoy")), root.path()),
            None
        );
        assert_eq!(
            installed_in(&bin.join(exe_name("envoy")), &root.path().join("missing")),
            None
        );
    }

    #[test]
    fn the_switch_turns_updating_off() {
        let lookup = |value: &'static str| move |_: &str| Some(value.to_string());

        assert!(switched_off(&lookup("off")));
        assert!(switched_off(&lookup("0")));
        assert!(!switched_off(&lookup("on")));
        assert!(!switched_off(&|_: &str| None));
    }

    #[test]
    fn a_look_less_than_a_day_ago_is_recent() {
        let dir = tempdir().expect("tempdir is created");
        let stamp = dir.path().join("last-check");
        assert!(!modified_within(&stamp, INTERVAL), "never looked");

        fs::write(&stamp, b"").expect("stamp is written");
        assert!(modified_within(&stamp, INTERVAL));

        let old = SystemTime::now() - INTERVAL - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&stamp)
            .and_then(|file| file.set_modified(old))
            .expect("stamp is backdated");
        assert!(!modified_within(&stamp, INTERVAL));
    }

    #[test]
    fn the_pointer_naming_this_version_means_nothing_to_do() {
        let share = share_with_pointer(&format!("{{\"schema\": 1, \"version\": \"{VERSION}\"}}"));
        let dir = tempdir().expect("tempdir is created");

        assert_eq!(
            update(share.path(), dir.path()),
            Ok(Outcome::Current(VERSION.to_string()))
        );
        assert_eq!(
            fs::read_dir(dir.path()).expect("dir reads").count(),
            0,
            "nothing is staged"
        );
    }

    #[test]
    fn a_pointer_that_cannot_be_trusted_is_refused() {
        let dir = tempdir().expect("tempdir is created");
        let cases = [
            ("{\"schema\": 2, \"version\": \"v9\"}", "newer launcher"),
            ("{\"version\": \"../../x\"}", "not a safe version"),
            ("{\"version\": \"\"}", "not a safe version"),
            ("{\"schema\": 1}", "not valid"),
            ("[1, 2]", "not valid"),
        ];
        for (contents, expected) in cases {
            let share = share_with_pointer(contents);
            let error = update(share.path(), dir.path()).expect_err(contents);
            assert!(error.contains(expected), "{contents}: {error}");
        }

        let missing = tempdir().expect("tempdir is created");
        let error = update(missing.path(), dir.path()).expect_err("no pointer");
        assert!(error.contains("cannot read"), "{error}");
    }

    #[test]
    fn a_pointer_with_a_byte_order_mark_is_read() {
        let share = share_with_pointer(&format!("\u{feff}{{\"version\": \"{VERSION}\"}}"));
        let dir = tempdir().expect("tempdir is created");

        assert!(matches!(
            update(share.path(), dir.path()),
            Ok(Outcome::Current(_))
        ));
    }

    #[test]
    fn replacing_a_launcher_leaves_only_the_new_one() {
        let dir = tempdir().expect("tempdir is created");
        let target = dir.path().join(exe_name("envoy"));
        let staged = dir.path().join("staged");
        fs::write(&target, "old").expect("old launcher is written");
        fs::write(&staged, "new").expect("staged launcher is written");

        replace(&target, &staged).expect("it is replaced");

        assert_eq!(fs::read_to_string(&target).expect("target reads"), "new");
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .expect("dir reads")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names, vec![exe_name("envoy"), "staged".to_string()]);
    }

    #[test]
    fn swapping_replaces_every_name_the_installer_made() {
        let dir = tempdir().expect("tempdir is created");
        let staged = dir.path().join("staged");
        fs::write(&staged, "new").expect("staged launcher is written");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).expect("bin is created");
        fs::write(bin.join(exe_name("envoy")), "old").expect("envoy is written");
        #[cfg(unix)]
        std::os::unix::fs::symlink("envoy", bin.join("en")).expect("en is linked");
        #[cfg(not(unix))]
        fs::write(bin.join("en.exe"), "old").expect("en is written");

        swap(&bin, &staged).expect("it is swapped");

        assert_eq!(
            fs::read_to_string(bin.join(exe_name("envoy"))).expect("envoy reads"),
            "new"
        );
        // Through the link on Unix, as a copy on Windows.
        assert_eq!(
            fs::read_to_string(bin.join(exe_name("en"))).expect("en reads"),
            "new"
        );
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(bin.join("en"))
                .expect("en exists")
                .file_type()
                .is_symlink(),
            "the link stays a link"
        );
    }

    #[test]
    fn an_install_without_en_is_still_swapped() {
        let dir = tempdir().expect("tempdir is created");
        let staged = dir.path().join("staged");
        fs::write(&staged, "new").expect("staged launcher is written");
        fs::write(dir.path().join(exe_name("envoy")), "old").expect("envoy is written");

        swap(dir.path(), &staged).expect("it is swapped");

        assert!(!dir.path().join(exe_name("en")).exists());
        assert_eq!(
            fs::read_to_string(dir.path().join(exe_name("envoy"))).expect("envoy reads"),
            "new"
        );
    }

    #[test]
    fn leftovers_are_noticed_and_cleaned_up() {
        let dir = tempdir().expect("tempdir is created");
        fs::write(dir.path().join(exe_name("envoy")), "launcher").expect("launcher is written");
        assert!(!has_leftovers(dir.path()), "a tidy folder has none");

        for stem in ["envoy", "en"] {
            for suffix in [".old", ".new"] {
                fs::write(beside(&dir.path().join(exe_name(stem)), suffix), "x")
                    .expect("leftover is written");
            }
        }
        fs::create_dir(dir.path().join(STAGING)).expect("staging is created");
        assert!(has_leftovers(dir.path()));

        cleanup(dir.path());

        let left: Vec<_> = fs::read_dir(dir.path())
            .expect("dir reads")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from(exe_name("envoy"))]);
        assert!(!has_leftovers(dir.path()));
    }

    #[test]
    fn only_one_launch_holds_the_lock_at_a_time() {
        let state = tempdir().expect("tempdir is created");

        let first = Lock::try_acquire(state.path()).expect("the lock is free");
        assert!(
            Lock::try_acquire(state.path()).is_none(),
            "a second launch leaves the work to the first"
        );

        drop(first);
        assert!(
            Lock::try_acquire(state.path()).is_some(),
            "the lock is free again once released"
        );
    }

    #[test]
    fn the_lock_is_made_where_it_does_not_yet_exist() {
        let state = tempdir().expect("tempdir is created");
        let nested = state.path().join("not").join("yet");

        let lock = Lock::try_acquire(&nested).expect("the folder is created");

        assert!(nested.join(LOCK).is_file());
        drop(lock);
        assert!(!nested.join(LOCK).exists(), "nothing is left behind");
    }

    #[test]
    fn a_lock_left_by_a_launch_that_was_killed_is_taken_over() {
        let state = tempdir().expect("tempdir is created");
        let _killed = Lock::try_acquire(state.path()).expect("the lock is free");
        let long_ago = SystemTime::now() - STALE_LOCK - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(state.path().join(LOCK))
            .and_then(|file| file.set_modified(long_ago))
            .expect("the lock is backdated");

        assert!(Lock::try_acquire(state.path()).is_some());
    }
}
