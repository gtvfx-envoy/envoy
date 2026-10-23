//! A light launcher for envoy.
//!
//! The shim is what a user runs as `envoy` or `en`. It reads the redirect file
//! on the share to learn which envoy version the user is pinned to, makes sure
//! that version is cached locally (copying it from the share the first time),
//! and runs it with the arguments it was given. One file on the share therefore
//! controls the version for a whole team, and no new envoy release is needed
//! for the shim to work with older versions.
//!
//! It is deliberately small: no network libraries, no telemetry, and nothing
//! that does not belong to finding and starting the right version.
//!
//! When the share cannot be reached it falls back to the last redirect file it
//! read and to the version it already cached, so envoy still starts offline.
//!
//! Options that belong to the shim itself start with `--envoy-shim-` and are
//! only recognised as the first argument; everything else is passed to envoy
//! untouched. See [`flags`].

mod app;
mod error;
mod flags;
mod identity;
mod launch;
mod source;
mod update;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use envoy_config::redirect::is_safe_version;
use envoy_store::settings::{self, Location};
use envoy_store::ProgressPrinter;

pub use error::ShimError;
pub use update::TARGET;

/// The version of this launcher.
pub const VERSION: &str = env!("ENVOY_SHIM_VERSION");

/// Names a version to run instead of the one the redirect file gives: a way
/// round the file for an emergency or a test.
pub const VERSION_VAR: &str = "ENVOY_VERSION";

/// Milliseconds to wait for the share before falling back to a saved copy.
pub const TIMEOUT_VAR: &str = "ENVOY_SHIM_TIMEOUT_MS";

/// Run the shim with the process's arguments (without the program name) and
/// return the exit code to end with.
pub fn run(args: Vec<OsString>) -> i32 {
    let env = |name: &str| std::env::var(name).ok();
    match args.first().and_then(|arg| arg.to_str()) {
        Some(flag) if flag.starts_with(flags::PREFIX) => flags::run(flag, &args[1..], &env),
        _ => forward(&args, &env),
    }
}

/// What the shim works out before it touches the app.
pub(crate) struct Decision {
    pub user: String,
    pub group: String,
    pub version: String,
    pub overridden: bool,
    /// The redirect file. `None` when `ENVOY_VERSION` names a version that is
    /// already cached: the file is not needed then, so the share is not read.
    pub loaded: Option<source::Loaded>,
    pub location: Location,
}

impl Decision {
    /// Where the redirect file says to look for the app.
    pub fn sources(&self) -> &[String] {
        match &self.loaded {
            Some(loaded) => loaded.redirect.sources.as_slice(),
            None => &[],
        }
    }
}

pub(crate) fn decide(env: &dyn Fn(&str) -> Option<String>) -> Result<Decision, ShimError> {
    let location = settings::from_environment();
    let user = identity::current_user(env);

    if let Some(version) = forced_version(env)? {
        // Naming a version is a way round the redirect file. The file is only
        // read, to learn where to copy the version from, when it is not here yet.
        let loaded = if app::cached(&version, &location).is_some() {
            None
        } else {
            let loaded = load_redirect(env, &location).map_err(|error| {
                ShimError::Redirect(format!(
                    "{VERSION_VAR} names {version}, which is not cached here, and the redirect \
file that says where to copy it from could not be used: {error}"
                ))
            })?;
            Some(loaded)
        };
        return Ok(Decision {
            user,
            group: format!("(set by {VERSION_VAR})"),
            version,
            overridden: true,
            loaded,
            location,
        });
    }

    let loaded = load_redirect(env, &location)?;
    let resolution = loaded.redirect.resolve(&user)?;
    Ok(Decision {
        user,
        group: resolution.group,
        version: resolution.version,
        overridden: false,
        loaded: Some(loaded),
        location,
    })
}

/// The version named by `ENVOY_VERSION`, if it is set.
fn forced_version(env: &dyn Fn(&str) -> Option<String>) -> Result<Option<String>, ShimError> {
    let Some(version) = env(VERSION_VAR)
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty())
    else {
        return Ok(None);
    };
    if !is_safe_version(&version) {
        return Err(ShimError::Redirect(format!(
            "{VERSION_VAR} is '{version}', which is not a safe version name"
        )));
    }
    Ok(Some(version))
}

/// Read the redirect file, or the copy saved from the last good one.
pub(crate) fn load_redirect(
    env: &dyn Fn(&str) -> Option<String>,
    location: &Location,
) -> Result<source::Loaded, ShimError> {
    let timeout = env(TIMEOUT_VAR)
        .and_then(|millis| millis.trim().parse::<u64>().ok())
        .map_or(source::DEFAULT_TIMEOUT, Duration::from_millis);
    // Without caching there is no cached app to fall back on, so no copy of the file is kept.
    let snapshots = location.enabled.then(|| location.root.join("snapshots"));
    source::load(env, snapshots.as_deref(), timeout)
}

/// Start envoy with `args`: resolve the version, make it runnable, hand over.
fn forward(args: &[OsString], env: &dyn Fn(&str) -> Option<String>) -> i32 {
    let float = std::env::current_exe().ok();

    // A nested call stays on the version that started it.
    if let Some(locked) = env(launch::BIN_LOCK_VAR)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        return launch::launch(&locked, args, float.as_deref());
    }

    let decision = decide(env);
    // Whatever came of the redirect file, now is the time to look for a newer
    // launcher: a file written for a newer launcher than this one is exactly
    // when an update is needed.
    update::maybe_update(env);
    let decision = match decision {
        Ok(decision) => decision,
        Err(error) => return fail(&error),
    };
    if let Some(source::Loaded {
        origin: source::Origin::Snapshot { age, reason, .. },
        ..
    }) = &decision.loaded
    {
        eprintln!(
            "envoy: the redirect file could not be used ({reason}); using the copy saved {} ago",
            how_long(*age)
        );
    }

    let app = match app::ensure(
        &decision.version,
        decision.sources(),
        env,
        &decision.location,
        &ProgressPrinter::new(),
    ) {
        Ok(app) => app,
        Err(error) => return fail(&error),
    };
    launch::launch(&app.exe, args, float.as_deref())
}

pub(crate) fn fail(error: &ShimError) -> i32 {
    eprintln!("envoy: {error}");
    error.exit_code()
}

/// A length of time for people, such as `3 hours`.
pub(crate) fn how_long(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let (number, unit) = match seconds {
        0..=59 => return "a moment".to_string(),
        60..=3_599 => (seconds / 60, "minute"),
        3_600..=86_399 => (seconds / 3_600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    format!("{number} {unit}{}", if number == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_written_for_people() {
        assert_eq!(how_long(Duration::from_secs(5)), "a moment");
        assert_eq!(how_long(Duration::from_secs(60)), "1 minute");
        assert_eq!(how_long(Duration::from_secs(3 * 3_600)), "3 hours");
        assert_eq!(how_long(Duration::from_secs(40 * 86_400)), "40 days");
    }
}
