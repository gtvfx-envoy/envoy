//! Runs the real shim against a fake share and a stand-in app.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use tempfile::{tempdir, TempDir};

#[path = "../../test_support/startable.rs"]
mod startable;

const SHIM: &str = env!("CARGO_BIN_EXE_envoy-shim");

const REDIRECT: &str = "\
schema: 1
sources:
  - ${STUDIO_SHARE}/envoy/{version}
versions:
  default: v1
  preview: v2
  ghost: v9
users:
  alice: preview
  zed: ghost
";

/// The stand-in app that cargo builds beside the test binaries.
fn fake_app() -> PathBuf {
    let shim = PathBuf::from(SHIM);
    let name = if cfg!(windows) {
        "fake_app.exe"
    } else {
        "fake_app"
    };
    let path = shim
        .parent()
        .expect("the shim has a directory")
        .join("examples")
        .join(name);
    assert!(
        path.is_file(),
        "{} is missing; `cargo test` builds it",
        path.display()
    );
    path
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "envoy.exe"
    } else {
        "envoy"
    }
}

/// The stand-in for a newer launcher that cargo builds beside the test binaries.
fn fake_shim() -> PathBuf {
    let name = if cfg!(windows) {
        "fake_shim.exe"
    } else {
        "fake_shim"
    };
    let path = PathBuf::from(SHIM)
        .parent()
        .expect("the shim has a directory")
        .join("examples")
        .join(name);
    assert!(
        path.is_file(),
        "{} is missing; `cargo test` builds it",
        path.display()
    );
    path
}

/// A share with the given versions published, a redirect file beside it, and
/// directories for the cache and for settings.
struct Studio {
    _temp: TempDir,
    share: PathBuf,
    config: PathBuf,
    cache: PathBuf,
    settings: PathBuf,
}

impl Studio {
    fn new(versions: &[&str]) -> Self {
        let temp = tempdir().expect("tempdir is created");
        let share = temp.path().join("share");
        let config = temp.path().join("config");
        for version in versions {
            let bin = share.join("envoy").join(version).join("bin");
            fs::create_dir_all(&bin).expect("bin is created");
            fs::copy(fake_app(), bin.join(exe_name())).expect("app is copied");
        }
        fs::create_dir_all(&config).expect("config is created");
        fs::write(config.join("redirect.yaml"), REDIRECT).expect("redirect is written");

        Self {
            cache: temp.path().join("cache"),
            settings: temp.path().join("settings"),
            _temp: temp,
            share,
            config,
        }
    }

    /// The shim, run as `user`, with nothing inherited from the real environment.
    fn shim(&self, user: &str) -> Command {
        self.command(Path::new(SHIM), user)
    }

    fn command(&self, program: &Path, user: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("ENVOY_USER", user)
            .env("ENVOY_CONFIG_PROD", &self.config)
            .env("ENVOY_CACHE_ROOT", &self.cache)
            .env("ENVOY_CONFIG_ROOT", &self.settings)
            .env("STUDIO_SHARE", &self.share);
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        command
    }

    fn bin(&self) -> PathBuf {
        self.settings.join("bin")
    }

    /// Install a copy of the shim where the installer puts it, as `envoy` and
    /// `en`, and return the path of `envoy`.
    fn install_shim(&self) -> PathBuf {
        fs::create_dir_all(self.bin()).expect("bin is created");
        let installed = self.bin().join(exe_name());
        startable::copy_startable(Path::new(SHIM), &installed, "--envoy-shim-version");
        #[cfg(unix)]
        std::os::unix::fs::symlink("envoy", self.bin().join("en")).expect("en is linked");
        #[cfg(not(unix))]
        fs::copy(SHIM, self.bin().join("en.exe")).expect("en is installed");
        installed
    }

    /// Publish the stand-in as launcher `version` on the share.
    fn publish_launcher(&self, version: &str) {
        let published = self
            .config
            .join("shim")
            .join(version)
            .join(envoy_shim::TARGET);
        fs::create_dir_all(&published).expect("published folder is created");
        fs::copy(fake_shim(), published.join(exe_name())).expect("launcher is published");
        self.point_at(version);
    }

    fn point_at(&self, version: &str) {
        fs::create_dir_all(self.config.join("shim")).expect("shim folder is created");
        fs::write(
            self.config.join("shim").join("current.json"),
            format!("{{\"schema\": 1, \"version\": \"{version}\"}}\n"),
        )
        .expect("pointer is written");
    }

    fn stamp(&self) -> PathBuf {
        self.settings.join("shim").join("last-check")
    }

    fn run(&self, user: &str, args: &[&str]) -> Output {
        self.shim(user).args(args).output().expect("the shim runs")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n")
}

/// The path the stand-in app says it was started from.
fn started_from(output: &Output) -> String {
    stdout(output)
        .lines()
        .find_map(|line| line.strip_prefix("exe=").map(str::to_string))
        .unwrap_or_else(|| panic!("the app printed no exe, stdout was:\n{}", stdout(output)))
}

fn in_directory(path: &str, directory: &Path) -> bool {
    Path::new(path).starts_with(directory)
}

fn backdate(path: &Path, by: Duration) {
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now() - by))
        .expect("the file is backdated");
}

#[test]
fn each_user_runs_the_version_the_redirect_file_gives_them() {
    let studio = Studio::new(&["v1", "v2"]);

    let alice = studio.run("alice", &[]);
    let bob = studio.run("BOB", &[]);

    assert!(alice.status.success(), "{}", stderr(&alice));
    assert!(started_from(&alice).contains("v2"), "alice is pinned to v2");
    assert!(started_from(&bob).contains("v1"), "bob gets the default");
    assert!(in_directory(&started_from(&alice), &studio.cache));
}

#[test]
fn arguments_reach_envoy_untouched_and_shim_options_only_count_first() {
    let studio = Studio::new(&["v1"]);

    let output = studio.run(
        "bob",
        &["--list", "two words", "--envoy-shim-version", "-x=1", "-"],
    );

    let printed = stdout(&output);
    for expected in [
        "arg=--list",
        "arg=two words",
        "arg=--envoy-shim-version",
        "arg=-x=1",
        "arg=-",
    ] {
        assert!(printed.contains(expected), "missing {expected}:\n{printed}");
    }
}

#[test]
fn the_exit_code_of_envoy_is_the_exit_code_of_the_shim() {
    let studio = Studio::new(&["v1"]);

    let output = studio
        .shim("bob")
        .env("FAKE_APP_EXIT", "7")
        .output()
        .expect("the shim runs");

    assert_eq!(output.status.code(), Some(7));
}

#[test]
fn the_first_run_copies_the_version_and_later_runs_do_not() {
    let studio = Studio::new(&["v1"]);

    let first = studio.run("bob", &[]);
    let second = studio.run("bob", &[]);

    assert!(
        stderr(&first).contains("caching app:envoy@v1"),
        "stderr was:\n{}",
        stderr(&first)
    );
    assert!(
        stderr(&second).is_empty(),
        "a warm start is silent, stderr was:\n{}",
        stderr(&second)
    );
    assert!(studio.cache.join("bundles/app/envoy/v1/bin").is_dir());
}

#[test]
fn an_app_that_reports_another_version_than_it_was_published_as_is_said_so_once() {
    let studio = Studio::new(&["v1"]);
    let run = |reported: &str| {
        studio
            .shim("bob")
            .env("FAKE_APP_VERSION", reported)
            .output()
            .expect("the shim runs")
    };

    let first = run("v0.7.0-51-g8ec805c");
    let second = run("v0.7.0-51-g8ec805c");

    assert!(first.status.success(), "it still runs: {}", stderr(&first));
    assert!(
        stderr(&first).contains("published as v1 reports itself as v0.7.0-51-g8ec805c"),
        "stderr was:\n{}",
        stderr(&first)
    );
    assert!(
        !stderr(&second).contains("reports itself"),
        "it is said when the app is copied, not at every start:\n{}",
        stderr(&second)
    );
}

#[test]
fn build_metadata_does_not_make_a_version_another_version() {
    let studio = Studio::new(&["v1"]);

    let output = studio
        .shim("bob")
        .env("FAKE_APP_VERSION", "1+g8ec805c")
        .output()
        .expect("the shim runs");

    assert!(
        !stderr(&output).contains("reports itself"),
        "stderr was:\n{}",
        stderr(&output)
    );
}

#[test]
fn a_cached_version_still_runs_with_the_share_and_the_redirect_file_gone() {
    let studio = Studio::new(&["v1"]);
    studio.run("bob", &[]);
    fs::remove_dir_all(&studio.share).expect("the share goes away");
    fs::remove_file(studio.config.join("redirect.yaml")).expect("the redirect file goes away");

    let offline = studio.run("bob", &[]);

    assert!(offline.status.success(), "{}", stderr(&offline));
    assert!(in_directory(&started_from(&offline), &studio.cache));
    assert!(
        stderr(&offline).contains("using the copy saved"),
        "stderr was:\n{}",
        stderr(&offline)
    );
}

#[test]
fn a_version_that_is_not_cached_and_not_on_the_share_fails_with_a_distinct_code() {
    let studio = Studio::new(&["v1"]);

    let output = studio.run("zed", &[]);

    assert_eq!(output.status.code(), Some(69));
    let message = stderr(&output);
    assert!(message.contains("envoy v9 is not cached"), "{message}");
    assert!(message.contains("looked in"), "{message}");
}

#[test]
fn a_change_to_the_redirect_file_takes_effect_at_the_next_launch() {
    let studio = Studio::new(&["v1", "v2"]);
    assert!(started_from(&studio.run("alice", &[])).contains("v2"));

    let moved = REDIRECT.replace("alice: preview", "alice: default");
    fs::write(studio.config.join("redirect.yaml"), moved).expect("the file is republished");

    assert!(started_from(&studio.run("alice", &[])).contains("v1"));
}

#[test]
fn the_version_can_be_forced_for_a_single_run() {
    let studio = Studio::new(&["v1", "v2"]);

    let forced = studio
        .shim("bob")
        .env("ENVOY_VERSION", "v2")
        .output()
        .expect("the shim runs");

    assert!(started_from(&forced).contains("v2"));

    let unsafe_version = studio
        .shim("bob")
        .env("ENVOY_VERSION", "../x")
        .output()
        .expect("the shim runs");
    assert_eq!(unsafe_version.status.code(), Some(78));
}

#[test]
fn a_nested_call_stays_on_the_version_that_started_it() {
    let studio = Studio::new(&["v1", "v2"]);
    let outer = studio.run("bob", &[]);
    let locked = started_from(&outer);

    let mut nested = studio.shim("alice");
    nested
        .env_remove("ENVOY_CONFIG_PROD")
        .env("ENVOY_BIN_LOCK", &locked);
    let output = nested.output().expect("the shim runs");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(started_from(&output), locked, "alice's pin does not apply");
    assert!(stdout(&output).contains(&format!("ENVOY_BIN_LOCK={locked}")));
}

#[test]
fn with_caching_off_envoy_runs_from_the_share() {
    let studio = Studio::new(&["v1"]);

    let output = studio
        .shim("bob")
        .env("ENVOY_CACHE", "off")
        .output()
        .expect("the shim runs");

    assert!(in_directory(&started_from(&output), &studio.share));
    assert!(!studio.cache.exists(), "an off switch creates no cache");
}

#[test]
fn without_a_redirect_file_the_shim_says_what_to_set() {
    let studio = Studio::new(&["v1"]);

    let output = studio
        .shim("bob")
        .env_remove("ENVOY_CONFIG_PROD")
        .output()
        .expect("the shim runs");

    assert_eq!(output.status.code(), Some(78));
    assert!(stderr(&output).contains("ENVOY_CONFIG_PROD"));
}

#[test]
fn resolve_explains_the_choice_before_and_after_the_version_is_cached() {
    let studio = Studio::new(&["v1", "v2"]);

    let before = stdout(&studio.run("alice", &["--envoy-shim-resolve"]));
    assert!(before.contains("user:     alice"), "{before}");
    assert!(before.contains("group:    preview (assigned)"), "{before}");
    assert!(before.contains("version:  v2"), "{before}");
    assert!(
        before.contains("not cached yet; it would be copied from"),
        "{before}"
    );

    let stranger = stdout(&studio.run("carol", &["--envoy-shim-resolve"]));
    assert!(
        stranger.contains("group:    default (not listed, so the default)"),
        "{stranger}"
    );

    studio.run("alice", &[]);
    let after = stdout(&studio.run("alice", &["--envoy-shim-resolve"]));
    assert!(after.contains("app:      cached at"), "{after}");
}

#[test]
fn prefetch_caches_a_version_without_running_it() {
    let studio = Studio::new(&["v1", "v2"]);

    let output = studio.run("bob", &["--envoy-shim-prefetch", "v2"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("envoy v2 is ready (cached at"));
    assert!(!stdout(&output).contains("arg="), "nothing was launched");
    assert!(studio.cache.join("bundles/app/envoy/v2/bin").is_dir());
}

#[test]
fn validate_config_accepts_a_good_file_and_rejects_a_bad_one() {
    let studio = Studio::new(&["v1"]);
    let good = studio.config.join("redirect.yaml");
    let bad = studio.config.join("bad.yaml");
    fs::write(
        &bad,
        "schema: 1\nsources: ['/x/{version}']\nversions: {preview: v2}\nusers: {alice: nope}\n",
    )
    .expect("bad file is written");

    let accepted = studio.run(
        "bob",
        &["--envoy-shim-validate-config", &good.display().to_string()],
    );
    assert!(accepted.status.success(), "{}", stderr(&accepted));
    assert!(stdout(&accepted).contains(": ok"));

    let rejected = studio.run(
        "bob",
        &["--envoy-shim-validate-config", &bad.display().to_string()],
    );
    assert_eq!(rejected.status.code(), Some(78));
    let printed = stdout(&rejected);
    assert!(printed.contains("has no 'default'"), "{printed}");
    assert!(
        printed.contains("group 'nope', which has no version"),
        "{printed}"
    );

    let missing = studio.run("bob", &["--envoy-shim-validate-config"]);
    assert_eq!(missing.status.code(), Some(64));
}

#[test]
fn checking_sources_warns_about_a_version_nobody_published() {
    let studio = Studio::new(&["v1"]);
    let file = studio.config.join("redirect.yaml");

    let output = studio.run(
        "bob",
        &[
            "--envoy-shim-validate-config",
            &file.display().to_string(),
            "--check-sources",
        ],
    );

    assert!(
        output.status.success(),
        "a missing version is only a warning"
    );
    let printed = stdout(&output);
    assert!(
        printed.contains("version v2 was not found on any source"),
        "{printed}"
    );
    assert!(!printed.contains("version v1 was not found"), "{printed}");
}

#[test]
fn a_version_named_by_hand_that_is_cached_needs_neither_the_redirect_file_nor_the_share() {
    let studio = Studio::new(&["v1"]);
    studio.run("bob", &[]);
    fs::remove_dir_all(&studio.share).expect("the share goes away");
    fs::remove_dir_all(&studio.config).expect("the redirect file goes away");
    let by_hand = |args: &[&str]| {
        studio
            .shim("bob")
            .env_remove("ENVOY_CONFIG_PROD")
            .env("ENVOY_VERSION", "v1")
            .args(args)
            .output()
            .expect("the shim runs")
    };

    let output = by_hand(&[]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(in_directory(&started_from(&output), &studio.cache));
    assert!(
        stderr(&output).is_empty(),
        "there is nothing to say, stderr was:\n{}",
        stderr(&output)
    );

    let resolved = stdout(&by_hand(&["--envoy-shim-resolve"]));
    assert!(resolved.contains("config:   not read"), "{resolved}");
    assert!(resolved.contains("app:      cached at"), "{resolved}");
}

#[test]
fn a_version_named_by_hand_that_is_not_cached_needs_the_redirect_file_to_say_where_it_is() {
    let studio = Studio::new(&["v1"]);

    let output = studio
        .shim("bob")
        .env_remove("ENVOY_CONFIG_PROD")
        .env("ENVOY_VERSION", "v1")
        .output()
        .expect("the shim runs");

    assert_eq!(output.status.code(), Some(78));
    let message = stderr(&output);
    assert!(
        message.contains("ENVOY_VERSION names v1, which is not cached here"),
        "{message}"
    );
    assert!(message.contains("ENVOY_CONFIG_PROD"), "{message}");
}

#[test]
fn a_broken_team_file_keeps_the_team_on_its_last_good_version() {
    let studio = Studio::new(&["v1", "v2"]);
    let team_file = studio.config.join("rigging").join("redirect.yaml");
    fs::create_dir_all(team_file.parent().expect("a parent")).expect("team folder is created");
    fs::write(&team_file, REDIRECT.replace("default: v1", "default: v2"))
        .expect("team file is written");
    let as_team = || {
        let mut command = studio.shim("bob");
        command.env("ENVOY_TEAM", "rigging");
        command.output().expect("the shim runs")
    };

    let first = as_team();
    assert!(
        started_from(&first).contains("v2"),
        "the team's file pins v2"
    );

    fs::write(&team_file, "schema: 1\nversions: {default: [v2\n").expect("someone breaks it");
    let second = as_team();

    assert!(second.status.success(), "{}", stderr(&second));
    assert!(
        started_from(&second).contains("v2"),
        "the studio's v1 must not stand in for the team's file"
    );
    assert!(
        stderr(&second).contains("using the copy saved"),
        "stderr was:\n{}",
        stderr(&second)
    );
    assert!(started_from(&studio.run("bob", &[])).contains("v1"));
}

#[test]
fn a_file_with_a_problem_still_serves_those_it_works_for() {
    let studio = Studio::new(&["v1", "v2"]);
    // `typo` is a group with no version, and nobody is given a default.
    fs::write(
        studio.config.join("redirect.yaml"),
        "schema: 1\nsources:\n  - ${STUDIO_SHARE}/envoy/{version}\nversions:\n  preview: v2\n\
users:\n  alice: preview\n  carol: typo\n",
    )
    .expect("the file is republished");

    let alice = studio.run("alice", &["--envoy-shim-resolve"]);
    assert!(alice.status.success(), "{}", stderr(&alice));
    let printed = stdout(&alice);
    assert!(printed.contains("version:  v2"), "{printed}");
    assert!(
        printed.contains("warning:  the redirect file has a problem:"),
        "{printed}"
    );

    let carol = studio.run("carol", &[]);
    assert_eq!(carol.status.code(), Some(78));
    assert!(
        stderr(&carol).contains("group 'typo'"),
        "{}",
        stderr(&carol)
    );
    let bob = studio.run("bob", &[]);
    assert_eq!(bob.status.code(), Some(78));
    assert!(
        stderr(&bob).contains("no 'default' version"),
        "{}",
        stderr(&bob)
    );

    // The file still says where to look, so a version can be fetched by name.
    let prefetch = studio.run("bob", &["--envoy-shim-prefetch", "v1"]);
    assert!(prefetch.status.success(), "{}", stderr(&prefetch));
    assert!(stdout(&prefetch).contains("envoy v1 is ready (cached at"));

    assert!(
        !studio.cache.join("snapshots").exists(),
        "a file with problems is not kept as the last good copy"
    );
}

#[test]
fn a_launcher_is_updated_even_when_the_redirect_file_is_for_a_newer_launcher() {
    let studio = Studio::new(&["v1"]);
    let installed = studio.install_shim();
    fs::write(
        studio.config.join("redirect.yaml"),
        "schema: 2\nsomething: new\n",
    )
    .expect("the file is republished");
    studio.publish_launcher("v9.9.9");

    let output = studio
        .command(&installed, "bob")
        .output()
        .expect("the shim runs");

    // Without a saved copy this launcher cannot say which version to run ...
    assert_eq!(output.status.code(), Some(78));
    let message = stderr(&output);
    assert!(message.contains("schema 2"), "{message}");
    // ... but it does find the launcher that can.
    assert!(
        message.contains("the launcher was updated to v9.9.9"),
        "{message}"
    );
    assert_eq!(
        fs::read(&installed).expect("the launcher reads"),
        fs::read(fake_shim()).expect("the stand-in reads")
    );
}

#[test]
fn with_a_saved_copy_a_newer_schema_runs_the_old_version_and_still_finds_the_update() {
    let studio = Studio::new(&["v1"]);
    let installed = studio.install_shim();
    let first = studio
        .command(&installed, "bob")
        .output()
        .expect("the shim runs");
    assert!(first.status.success(), "{}", stderr(&first));

    // The team moves to a schema this launcher does not know and publishes the launcher that does.
    fs::write(
        studio.config.join("redirect.yaml"),
        "schema: 2\nsomething: new\n",
    )
    .expect("the file is republished");
    studio.publish_launcher("v9.9.9");
    backdate(&studio.stamp(), Duration::from_secs(25 * 60 * 60));

    let output = studio
        .command(&installed, "bob")
        .output()
        .expect("the shim runs");

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        started_from(&output).contains("v1"),
        "the saved copy still pins v1"
    );
    let message = stderr(&output);
    assert!(message.contains("using the copy saved"), "{message}");
    assert!(message.contains("schema 2"), "{message}");
    assert!(
        message.contains("the launcher was updated to v9.9.9"),
        "{message}"
    );
}

#[test]
fn a_launch_does_not_update_while_another_holds_the_lock() {
    let studio = Studio::new(&["v1"]);
    let installed = studio.install_shim();
    let original = fs::read(&installed).expect("the launcher reads");
    studio.publish_launcher("v9.9.9");
    let lock = studio.settings.join("shim").join("update.lock");
    fs::create_dir_all(lock.parent().expect("a parent")).expect("state folder is created");
    fs::write(&lock, b"").expect("another launch holds the lock");

    let held = studio
        .command(&installed, "bob")
        .output()
        .expect("the shim runs");

    assert!(held.status.success(), "{}", stderr(&held));
    assert_eq!(fs::read(&installed).expect("reads"), original);
    assert!(
        !studio.stamp().exists(),
        "it left the looking to the other launch"
    );
    assert!(lock.exists(), "a lock that is not ours is left alone");

    // A lock left by a launch that was killed does not stop updates for ever.
    backdate(&lock, Duration::from_secs(11 * 60));
    let stale = studio
        .command(&installed, "bob")
        .output()
        .expect("the shim runs");

    assert!(stale.status.success(), "{}", stderr(&stale));
    assert_eq!(
        fs::read(&installed).expect("reads"),
        fs::read(fake_shim()).expect("the stand-in reads")
    );
    assert!(!lock.exists(), "released when the update was done");
}

#[test]
fn leftovers_of_an_earlier_swap_are_cleared_by_the_next_launch() {
    let studio = Studio::new(&["v1"]);
    let installed = studio.install_shim();
    let original = fs::read(&installed).expect("the launcher reads");
    // Looked at today, so nothing is due: only the leftovers bring the launcher to the folder.
    fs::create_dir_all(studio.settings.join("shim")).expect("state folder is created");
    fs::write(studio.stamp(), b"").expect("the stamp is written");
    let stems = if cfg!(windows) {
        ["envoy.exe", "en.exe"]
    } else {
        ["envoy", "en"]
    };
    let mut leftovers = Vec::new();
    for stem in stems {
        for suffix in [".old", ".new"] {
            let path = studio.bin().join(format!("{stem}{suffix}"));
            fs::write(&path, "left behind").expect("leftover is written");
            leftovers.push(path);
        }
    }
    let staging = studio.bin().join(".update");
    fs::create_dir_all(&staging).expect("staging is created");

    let output = studio
        .command(&installed, "bob")
        .output()
        .expect("the shim runs");

    assert!(output.status.success(), "{}", stderr(&output));
    for path in leftovers {
        assert!(!path.exists(), "{} was left", path.display());
    }
    assert!(!staging.exists());
    assert_eq!(
        fs::read(&installed).expect("reads"),
        original,
        "the launcher itself is untouched"
    );
}

#[test]
fn the_shim_reports_its_own_version_and_help() {
    let studio = Studio::new(&["v1"]);

    assert!(stdout(&studio.run("bob", &["--envoy-shim-version"])).starts_with("envoy-shim "));
    assert!(stdout(&studio.run("bob", &["--envoy-shim-help"])).contains("--envoy-shim-resolve"));
    assert_eq!(
        studio.run("bob", &["--envoy-shim-nonsense"]).status.code(),
        Some(64)
    );
}

#[test]
fn a_newer_launcher_on_the_share_replaces_the_installed_one() {
    let studio = Studio::new(&["v1"]);
    let installed = studio.install_shim();
    studio.publish_launcher("v9.9.9");

    let output = studio
        .command(&installed, "bob")
        .output()
        .expect("the shim runs");

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        started_from(&output).contains("v1"),
        "this launch still runs the app"
    );
    assert!(
        stderr(&output).contains("the launcher was updated to v9.9.9"),
        "stderr was:\n{}",
        stderr(&output)
    );
    let replacement = fs::read(fake_shim()).expect("the stand-in reads");
    for name in [exe_name(), if cfg!(windows) { "en.exe" } else { "en" }] {
        assert_eq!(
            fs::read(studio.bin().join(name)).expect("the launcher reads"),
            replacement,
            "{name} was not replaced"
        );
    }
    #[cfg(unix)]
    assert!(
        fs::symlink_metadata(studio.bin().join("en"))
            .expect("en exists")
            .file_type()
            .is_symlink(),
        "en stays a link to envoy"
    );
    assert!(studio.stamp().is_file());
    assert!(
        !studio.bin().join(".update").exists(),
        "the staging folder is removed"
    );
}

#[test]
fn the_share_is_looked_at_once_a_day() {
    let studio = Studio::new(&["v1"]);
    let installed = studio.install_shim();
    let original = fs::read(&installed).expect("the launcher reads");
    studio.point_at("v9.9.9");

    // Nothing is published yet, so the first look finds nothing to install.
    let first = studio.command(&installed, "bob").output().expect("runs");
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(studio.stamp().is_file(), "the look is recorded");
    assert_eq!(fs::read(&installed).expect("reads"), original);

    // Published now, but the share was looked at today, so it is not looked at again.
    studio.publish_launcher("v9.9.9");
    let second = studio.command(&installed, "bob").output().expect("runs");
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(fs::read(&installed).expect("reads"), original);

    let yesterday = SystemTime::now() - Duration::from_secs(25 * 60 * 60);
    fs::File::options()
        .write(true)
        .open(studio.stamp())
        .and_then(|file| file.set_modified(yesterday))
        .expect("the stamp is backdated");
    let third = studio.command(&installed, "bob").output().expect("runs");
    assert!(third.status.success(), "{}", stderr(&third));
    assert_eq!(
        fs::read(&installed).expect("reads"),
        fs::read(fake_shim()).expect("the stand-in reads")
    );
}

#[test]
fn a_launcher_that_is_not_the_version_the_pointer_names_is_not_installed() {
    let studio = Studio::new(&["v1"]);
    let installed = studio.install_shim();
    let original = fs::read(&installed).expect("the launcher reads");
    studio.publish_launcher("v9.9.9");

    let output = studio
        .command(&installed, "bob")
        .env("FAKE_SHIM_VERSION", "v1.0.0")
        .output()
        .expect("the shim runs");

    assert!(
        output.status.success(),
        "a refused update never stops a launch"
    );
    assert_eq!(fs::read(&installed).expect("reads"), original);
    assert!(!stderr(&output).contains("updated"), "{}", stderr(&output));
    assert!(!studio.bin().join(".update").exists());
}

#[test]
fn updating_can_be_switched_off() {
    let studio = Studio::new(&["v1"]);
    let installed = studio.install_shim();
    let original = fs::read(&installed).expect("the launcher reads");
    studio.publish_launcher("v9.9.9");

    let output = studio
        .command(&installed, "bob")
        .env("ENVOY_SHIM_UPDATE", "off")
        .output()
        .expect("the shim runs");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(fs::read(&installed).expect("reads"), original);
    assert!(!studio.stamp().exists(), "it did not even look");
}

#[test]
fn a_launcher_that_is_not_installed_never_replaces_itself() {
    let studio = Studio::new(&["v1"]);
    studio.publish_launcher("v9.9.9");

    let output = studio.run("bob", &[]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stderr(&output).contains("updated"), "{}", stderr(&output));
    assert!(!studio.stamp().exists());
}

#[test]
fn updating_by_hand_says_what_it_found() {
    let studio = Studio::new(&["v1"]);
    let installed = studio.install_shim();

    // The pointer names the version this launcher already is.
    let own = stdout(&studio.run("bob", &["--envoy-shim-version"]));
    let own = own.trim().trim_start_matches("envoy-shim ");
    studio.point_at(own);
    let current = studio
        .command(&installed, "bob")
        .arg("--envoy-shim-update")
        .output()
        .expect("the shim runs");
    assert!(current.status.success(), "{}", stderr(&current));
    assert!(
        stdout(&current).contains("is current"),
        "{}",
        stdout(&current)
    );

    studio.publish_launcher("v9.9.9");
    let updated = studio
        .command(&installed, "bob")
        .arg("--envoy-shim-update")
        .output()
        .expect("the shim runs");
    assert!(updated.status.success(), "{}", stderr(&updated));
    assert!(
        stdout(&updated).contains("to v9.9.9"),
        "{}",
        stdout(&updated)
    );

    let from_a_build = studio.run("bob", &["--envoy-shim-update"]);
    assert!(from_a_build.status.success());
    assert!(
        stdout(&from_a_build).contains("does not update itself"),
        "{}",
        stdout(&from_a_build)
    );
}

#[test]
fn resolve_says_whether_the_launcher_updates_itself() {
    let studio = Studio::new(&["v1"]);
    let installed = studio.install_shim();

    let from_a_build = stdout(&studio.run("bob", &["--envoy-shim-resolve"]));
    assert!(
        from_a_build.contains("not installed in")
            && from_a_build.contains("does not update itself"),
        "{from_a_build}"
    );

    let installed_run = studio
        .command(&installed, "bob")
        .arg("--envoy-shim-resolve")
        .output()
        .expect("the shim runs");
    let printed = stdout(&installed_run);
    assert!(printed.contains("installed in"), "{printed}");
    assert!(
        printed.contains("has not looked for an update yet"),
        "{printed}"
    );
}
