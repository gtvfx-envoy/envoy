//! Runs the real shim against a fake share and a stand-in app.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::{tempdir, TempDir};

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
        let mut command = Command::new(SHIM);
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
fn the_shim_reports_its_own_version_and_help() {
    let studio = Studio::new(&["v1"]);

    assert!(stdout(&studio.run("bob", &["--envoy-shim-version"])).starts_with("envoy-shim "));
    assert!(stdout(&studio.run("bob", &["--envoy-shim-help"])).contains("--envoy-shim-resolve"));
    assert_eq!(
        studio.run("bob", &["--envoy-shim-nonsense"]).status.code(),
        Some(64)
    );
}
