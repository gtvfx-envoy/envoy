use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use assert_cmd::Command;
use envoy_core::bundle_cache::BundleCache;
use envoy_core::user_config::UserConfig;

fn stdout_text(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

fn stderr_text(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(prefix: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("{prefix}_{unique}_{}", std::process::id()));
        fs::create_dir_all(&path).expect("scratch dir should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn base_command() -> Command {
    let mut command = Command::cargo_bin("envoy").expect("envoy binary should build");
    command
        .env_remove("ENVOY_BNDL_ROOTS")
        .env_remove("ENVOY_STACK")
        .env_remove("ENVOY_STACK_CONTEXT")
        .env_remove("ENVOY_STACK_ROOTS")
        .env_remove("ENVOY_CONFIG_ROOT")
        .env_remove("ENVOY_COMMANDS_FILE")
        .env_remove("ENVOY_ALLOWLIST")
        .env_remove("ENVOY_TELEMETRY_ENABLED")
        .env_remove("ENVOY_TELEMETRY_ENDPOINT")
        .env_remove("ENVOY_TELEMETRY_REDACT_ARGS")
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_HEADERS")
        .env_remove("OTEL_EXPORTER_OTLP_TIMEOUT")
        .env_remove("OTEL_SERVICE_NAME")
        .env_remove("OTEL_RESOURCE_ATTRIBUTES");
    command
}

fn write_stack(path: &Path, namespace: &str, bundle: &Path) {
    let bundle_path = bundle.to_string_lossy().replace('\'', "''");
    fs::write(
        path,
        format!("namespace: {namespace}\nbundles:\n  - path: '{bundle_path}'\n"),
    )
    .expect("stack should be written");
}

/// All JSON files dropped under `dir`, sorted by filename (chronological,
/// since telemetry file-drop names are timestamp-prefixed).
fn telemetry_files_in(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
}

#[test]
fn help_lists_expected_flags() {
    let assert = base_command().arg("--help").assert().success();
    let stdout = stdout_text(&assert);

    for expected in [
        "--list",
        "--info <COMMAND>",
        "--which <COMMAND>",
        "--commands-file <PATH>",
        "--stack <NAME_OR_PATH>",
        "--set-stack <NAME_OR_PATH>",
        "--get-stack",
        "--list-stacks",
        "--override-bundle <BNDLID=PATH>",
        "--set-config <KEY=VALUE>",
        "--get-config [<KEY>]",
        "--list-configs",
        "--ignore-config",
        "--env <ENV_COMMAND>",
        "--tag <TAG>",
        "--incognito",
        "--shell",
        "--trace <VAR>",
        "-cf",
        "-s",
        "-o",
    ] {
        assert!(
            stdout.contains(expected),
            "expected help to mention {expected}, got:\n{stdout}"
        );
    }
}

#[test]
fn legacy_bundles_config_flag_is_rejected() {
    let assert = base_command()
        .args(["--bundles-config", "legacy.json"])
        .assert()
        .failure();
    let stderr = stderr_text(&assert);

    assert!(
        stderr.contains("unexpected argument '--bundles-config'"),
        "stderr was:\n{stderr}"
    );
}

#[test]
fn version_prints_a_version_string() {
    let assert = base_command().arg("--version").assert().success();
    let stdout = stdout_text(&assert);

    assert!(stdout.starts_with("envoy "), "stdout was:\n{stdout}");
}

#[test]
fn list_configs_runs_without_error() {
    let scratch = ScratchDir::new("envoy_list_configs");

    base_command()
        .arg("--list-configs")
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .success();
}

#[test]
fn set_config_and_get_config_round_trip() {
    let scratch = ScratchDir::new("envoy_user_config");
    let config_path = scratch.path().join("user_config.json");

    let set_assert = base_command()
        .args(["--set-config", "verbosity=verbose"])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .success();
    let set_stdout = stdout_text(&set_assert);
    assert!(set_stdout.contains("Saved: verbosity = \"verbose\""));

    let get_assert = base_command()
        .args(["--get-config", "verbosity"])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .success();
    let get_stdout = stdout_text(&get_assert);
    assert!(get_stdout.contains("verbosity = \"verbose\""));
    assert!(config_path.is_file());
}

#[test]
fn stack_config_can_be_set_read_and_cleared_through_cli() {
    let scratch = ScratchDir::new("envoy_stack_config");
    let stack_path = scratch.path().join("studio.estack");
    let stack_value = stack_path.to_string_lossy();

    base_command()
        .args(["--set-config", &format!("stack={stack_value}")])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .success();

    let get_assert = base_command()
        .args(["--get-config", "stack"])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .success();
    assert!(stdout_text(&get_assert).contains("stack ="));
    let persisted_config = UserConfig::load(Some(scratch.path().join("user_config.json")));
    assert_eq!(persisted_config.get("stack"), Some(stack_value.as_ref()));

    base_command()
        .args(["--set-config", "stack="])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .success();

    let cleared_assert = base_command()
        .args(["--get-config", "stack"])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .success();
    assert!(stdout_text(&cleared_assert).contains("stack: <not set>"));
}

fn write_minimal_checkout_bundle(bundle_root: &Path) {
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    let envoy_dir = bundle_root.join(".envoy");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"known": {"environment": []}}"#,
    )
    .expect("commands.json should be written");
}

/// `--docs` with no bundle argument opens envoy's own docs and is not
/// covered by an integration test here (it spawns a real OS file-open
/// command) -- see `app::tests::resolve_bundle_docs_target_*` in
/// `src/app.rs` for unit coverage of the bundle-scoped resolution logic
/// without that side effect. These two cases stay integration-testable
/// because both return an error before ever reaching the OS-open call.
#[test]
fn docs_reports_bundle_not_found() {
    let scratch = ScratchDir::new("envoy_docs_bundle_missing");
    let bundle_root = scratch.path().join("gt").join("maya");
    write_minimal_checkout_bundle(&bundle_root);
    let stack_path = scratch.path().join("studio.estack");
    write_stack(&stack_path, "bfd", &bundle_root);

    let assert = base_command()
        .arg("--stack")
        .arg(&stack_path)
        .args(["--docs", "gt:does-not-exist"])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .failure();
    let stderr = stderr_text(&assert);
    assert!(
        stderr.contains("Bundle 'gt:does-not-exist' not found"),
        "stderr was:\n{stderr}"
    );
}

#[test]
fn docs_reports_no_docs_found_for_a_bundle_without_docs_or_readme() {
    let scratch = ScratchDir::new("envoy_docs_no_docs");
    let bundle_root = scratch.path().join("gt").join("maya");
    write_minimal_checkout_bundle(&bundle_root);
    let stack_path = scratch.path().join("studio.estack");
    write_stack(&stack_path, "bfd", &bundle_root);

    let assert = base_command()
        .arg("--stack")
        .arg(&stack_path)
        .args(["--docs", "gt:maya"])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .failure();
    let stderr = stderr_text(&assert);
    assert!(
        stderr.contains("No docs found for 'gt:maya'"),
        "stderr was:\n{stderr}"
    );
}

#[test]
fn set_stack_validates_and_persists_a_real_stack_path() {
    let scratch = ScratchDir::new("envoy_set_stack_valid");
    let bundle_root = scratch.path().join("gt").join("maya");
    write_minimal_checkout_bundle(&bundle_root);
    let stack_path = scratch.path().join("studio.estack");
    write_stack(&stack_path, "bfd", &bundle_root);
    let stack_value = stack_path.to_string_lossy();

    let set_assert = base_command()
        .args(["--set-stack", &stack_value])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .success();
    assert!(stdout_text(&set_assert).contains("Saved default stack"));

    let persisted_config = UserConfig::load(Some(scratch.path().join("user_config.json")));
    assert_eq!(persisted_config.get("stack"), Some(stack_value.as_ref()));

    let get_assert = base_command()
        .args(["--get-stack"])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .success();
    let get_stdout = stdout_text(&get_assert);
    assert!(
        get_stdout.contains("Stack: studio"),
        "stdout was:\n{get_stdout}"
    );
    // `Stack::current()` reports a canonicalized path with any Windows
    // extended-length `\\?\` prefix stripped (see `resolve_input_path`/
    // `normalize_windows_path` in envoy-core), so canonicalize
    // `stack_path` the same way here too, to avoid flakiness if the raw
    // and canonicalized forms differ (e.g. a symlinked temp dir).
    let canonical_stack_path = fs::canonicalize(&stack_path)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| stack_path.display().to_string());
    let expected_path = canonical_stack_path
        .strip_prefix(r"\\?\")
        .unwrap_or(&canonical_stack_path);
    assert!(
        get_stdout.contains(expected_path),
        "stdout was:\n{get_stdout}"
    );
}

#[test]
fn set_stack_rejects_an_unresolvable_value_without_persisting() {
    let scratch = ScratchDir::new("envoy_set_stack_invalid");

    let assert = base_command()
        .args(["--set-stack", "definitely-not-a-real-stack"])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .failure();
    let stderr = stderr_text(&assert);
    assert!(stderr.contains("not found"), "stderr was:\n{stderr}");

    let persisted_config = UserConfig::load(Some(scratch.path().join("user_config.json")));
    assert_eq!(persisted_config.get("stack"), None);
}

#[test]
fn get_stack_reports_none_selected_when_nothing_is_configured() {
    let scratch = ScratchDir::new("envoy_get_stack_none");

    let assert = base_command()
        .args(["--get-stack"])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .assert()
        .success();
    assert!(stdout_text(&assert).contains("No stack currently selected"));
}

#[test]
fn list_stacks_reports_none_found_with_empty_stack_roots() {
    let scratch = ScratchDir::new("envoy_list_stacks_empty");

    let assert = base_command()
        .args(["--list-stacks"])
        .env("ENVOY_CONFIG_ROOT", scratch.path())
        .env("ENVOY_STACK_ROOTS", "")
        .assert()
        .success();
    assert!(stdout_text(&assert).contains("No named stacks found"));
}

#[test]
fn raw_absolute_path_executable_runs_successfully() {
    #[cfg(windows)]
    let comspec =
        env::var("ComSpec").unwrap_or_else(|_| String::from(r"C:\Windows\System32\cmd.exe"));
    #[cfg(windows)]
    let args = [comspec.as_str(), "/c", "exit", "0"];

    #[cfg(not(windows))]
    let args = ["/bin/sh", "-c", "exit 0"];

    base_command().args(args).assert().success();
}

#[test]
fn unregistered_command_name_returns_not_found() {
    let scratch = ScratchDir::new("envoy_missing_command");
    let envoy_dir = scratch.path().join(".envoy");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{
  "known": {
    "environment": []
  }
}"#,
    )
    .expect("commands.json should be written");

    let assert = base_command()
        .arg("missing_command")
        .current_dir(scratch.path())
        .assert()
        .failure();
    let stderr = stderr_text(&assert);

    assert!(
        stderr.contains("Error: Command 'missing_command' not found"),
        "stderr was:\n{stderr}"
    );
}

#[test]
fn command_info_reports_target_and_platform_resolution() {
    let scratch = ScratchDir::new("envoy_platform_info");
    let envoy_dir = scratch.path().join(".envoy");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    let commands_text = format!(
        r#"{{
  "known": {{
    "environment": [],
    "alias": ["base-tool"],
    "platforms": {{
      "{}": {{
        "alias": ["os-tool"],
        "architectures": {{
          "{}": {{"alias": ["target-tool"]}}
        }}
      }}
    }}
  }}
}}"#,
        env::consts::OS,
        env::consts::ARCH
    );
    fs::write(envoy_dir.join("commands.json"), commands_text)
        .expect("commands.json should be written");

    let assert = base_command()
        .args(["--info", "known"])
        .current_dir(scratch.path())
        .assert()
        .success();
    let stdout = stdout_text(&assert);

    assert!(
        stdout.contains(&format!(
            "Target: {}/{}",
            env::consts::OS,
            env::consts::ARCH
        )),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "Configuration: base -> {} -> {}",
            env::consts::OS,
            env::consts::ARCH
        )),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("Executable: target-tool"),
        "stdout was:\n{stdout}"
    );
}

/// End-to-end coverage for the Phase 1-3 wiring gaps closed in this pass:
/// a *published* bundle whose `.envoy/team.json` and `.envoy/commands.json`
/// come from a warm bundle-cache entry, all selected through a strict stack
/// resolved automatically through the real `envoy` binary -- not just via
/// direct, isolated unit calls to each module.
#[test]
fn verbose_run_resolves_stack_team_config_and_warm_bundle_cache_together() {
    let scratch = ScratchDir::new("envoy_full_wiring");

    // A published bundle (has a `.bundle` marker) discovered via
    // ENVOY_BNDL_ROOTS. Its own commands.json defines a command that should
    // NOT end up loaded, because a cached snapshot takes precedence.
    let bundle_root = scratch.path().join("gt").join("maya");
    let envoy_dir = bundle_root.join(".envoy");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(bundle_root.join(".bundle"), r#"{"version": "2.0.0"}"#)
        .expect(".bundle marker should be written");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"from_checkout_root": {"environment": []}}"#,
    )
    .expect("original commands.json should be written");

    // The cached snapshot: a *different* commands.json (so we can prove it
    // -- not the original bundle root -- is what actually gets loaded), plus
    // the team.json that should be auto-resolved.
    let cached_source = scratch.path().join("cached_source");
    let cached_envoy_dir = cached_source.join(".envoy");
    fs::create_dir_all(&cached_envoy_dir).expect("cached .envoy dir should be created");
    fs::write(
        cached_envoy_dir.join("commands.json"),
        r#"{"known_from_cache": {"environment": []}}"#,
    )
    .expect("cached commands.json should be written");
    fs::write(cached_envoy_dir.join("team.json"), r#"{"name": "bfd"}"#)
        .expect("cached team.json should be written");
    let stack_path = scratch.path().join("studio.estack");
    write_stack(&stack_path, "bfd", &bundle_root);

    let cache_root = scratch.path().join("bundle_cache");
    let mut cache = BundleCache::new(&cache_root).expect("bundle cache should open");
    cache
        .store("gt:maya", "1.0.0", &cached_source)
        .expect("storing the cached snapshot should succeed");

    let assert = base_command()
        .args(["--verbose", "--stack"])
        .arg(&stack_path)
        .arg("--list")
        .env("ENVOY_BUNDLE_CACHE", &cache_root)
        .assert()
        .success();

    let stdout = stdout_text(&assert);
    let stderr = stderr_text(&assert);

    assert!(
        stdout.contains("known_from_cache"),
        "expected the warm cache's commands to be loaded, stdout was:\n{stdout}"
    );
    assert!(
        !stdout.contains("from_checkout_root"),
        "the original bundle's commands should be shadowed once the cache is \
warm, stdout was:\n{stdout}"
    );
    assert!(
        stderr.contains("debug: Resolved team config: bfd"),
        "stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("debug: Resolved stack: studio"),
        "stderr was:\n{stderr}"
    );
}

/// A developer's own checkout must never be silently swapped for a cached
/// snapshot, even when a bundle-cache entry exists under the same bndlid.
#[test]
fn verbose_run_never_substitutes_a_checkout_bundle_for_a_cache_entry() {
    let scratch = ScratchDir::new("envoy_checkout_not_cached");

    // A checkout bundle: `.git` marker only, no `.bundle` marker.
    let bundle_root = scratch.path().join("gt").join("maya");
    let envoy_dir = bundle_root.join(".envoy");
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"from_checkout_root": {"environment": []}}"#,
    )
    .expect("original commands.json should be written");

    // A cache entry under the same bndlid that must be ignored.
    let cached_source = scratch.path().join("cached_source");
    let cached_envoy_dir = cached_source.join(".envoy");
    fs::create_dir_all(&cached_envoy_dir).expect("cached .envoy dir should be created");
    fs::write(
        cached_envoy_dir.join("commands.json"),
        r#"{"known_from_cache": {"environment": []}}"#,
    )
    .expect("cached commands.json should be written");

    let cache_root = scratch.path().join("bundle_cache");
    let mut cache = BundleCache::new(&cache_root).expect("bundle cache should open");
    cache
        .store("gt:maya", "1.0.0", &cached_source)
        .expect("storing the cache entry should succeed");

    let assert = base_command()
        .args(["--verbose", "--list"])
        .env("ENVOY_BNDL_ROOTS", scratch.path())
        .env("ENVOY_BUNDLE_CACHE", &cache_root)
        .assert()
        .success();

    let stdout = stdout_text(&assert);

    assert!(
        stdout.contains("from_checkout_root"),
        "the developer's own checkout should still be loaded, stdout was:\n{stdout}"
    );
    assert!(
        !stdout.contains("known_from_cache"),
        "a checkout bundle must never be substituted for a cached snapshot, \
stdout was:\n{stdout}"
    );
}

#[test]
fn diagnose_without_command_summarizes_stack_bundles_and_team() {
    let scratch = ScratchDir::new("envoy_diagnose_summary");

    let bundle_root = scratch.path().join("gt").join("maya");
    let envoy_dir = bundle_root.join(".envoy");
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"known": {"environment": []}}"#,
    )
    .expect("commands.json should be written");
    fs::write(envoy_dir.join("team.json"), r#"{"name": "bfd"}"#)
        .expect("team.json should be written");
    let stack_path = scratch.path().join("studio.estack");
    write_stack(&stack_path, "bfd", &bundle_root);

    let cache_root = scratch.path().join("bundle_cache");

    let assert = base_command()
        .args(["--stack"])
        .arg(&stack_path)
        .arg("--diagnose")
        .env("ENVOY_BUNDLE_CACHE", &cache_root)
        .assert()
        .success();
    let stdout = stdout_text(&assert);

    assert!(stdout.contains("envoy diagnose"), "stdout was:\n{stdout}");
    assert!(
        stdout.contains("Bundles discovered: 1"),
        "stdout was:\n{stdout}"
    );
    assert!(stdout.contains("gt:maya"), "stdout was:\n{stdout}");
    assert!(
        stdout.contains("Commands registered: 1"),
        "stdout was:\n{stdout}"
    );
    assert!(stdout.contains("Team config: bfd"), "stdout was:\n{stdout}");
    assert!(
        stdout.contains("Current stack: studio"),
        "stdout was:\n{stdout}"
    );
    assert!(stdout.contains("Bundle cache:"), "stdout was:\n{stdout}");
    assert!(stdout.contains("VCS detected:"), "stdout was:\n{stdout}");
    assert!(
        stdout.contains("Telemetry (automatic envoy.command.run export): disabled"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("Bundle root reachability:"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("Pass a COMMAND"),
        "should prompt for a command to see its resolved environment, stdout was:\n{stdout}"
    );
}

#[test]
fn diagnose_with_command_shows_resolved_environment() {
    let scratch = ScratchDir::new("envoy_diagnose_command");

    let bundle_root = scratch.path().join("gt").join("maya");
    let envoy_dir = bundle_root.join(".envoy");
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"known": {"environment": ["known_env.json"]}}"#,
    )
    .expect("commands.json should be written");
    fs::write(
        envoy_dir.join("known_env.json"),
        r#"{"DIAGNOSE_SMOKE_TEST": "hello"}"#,
    )
    .expect("known_env.json should be written");

    let assert = base_command()
        .args(["--diagnose", "known"])
        .env("ENVOY_BNDL_ROOTS", scratch.path())
        .assert()
        .success();
    let stdout = stdout_text(&assert);

    assert!(
        stdout.contains("Environment resolution for 'known'"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("DIAGNOSE_SMOKE_TEST = hello"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("Use --trace VAR known"),
        "stdout was:\n{stdout}"
    );
}

#[test]
fn shell_mode_launches_a_shell_with_the_resolved_environment_applied() {
    let scratch = ScratchDir::new("envoy_shell_mode");

    let bundle_root = scratch.path().join("gt").join("maya");
    let envoy_dir = bundle_root.join(".envoy");
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"mytool": {"environment": ["mytool_env.json"]}}"#,
    )
    .expect("commands.json should be written");
    fs::write(
        envoy_dir.join("mytool_env.json"),
        r#"{"SHELL_MODE_MARKER": "hello-from-shell-mode"}"#,
    )
    .expect("mytool_env.json should be written");

    #[cfg(windows)]
    let shell_input = "echo %SHELL_MODE_MARKER%\r\nexit\r\n";
    #[cfg(not(windows))]
    let shell_input = "echo $SHELL_MODE_MARKER\nexit\n";

    let assert = base_command()
        .args(["--shell", "mytool"])
        .env("ENVOY_BNDL_ROOTS", scratch.path())
        .write_stdin(shell_input)
        .assert()
        .success();
    let stdout = stdout_text(&assert);

    assert!(
        stdout.contains("Entering shell inside mytool's resolved environment"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("hello-from-shell-mode"),
        "stdout was:\n{stdout}"
    );
}

#[test]
fn shell_mode_reports_command_not_found() {
    let scratch = ScratchDir::new("envoy_shell_missing_command");
    let bundle_root = scratch.path().join("gt").join("maya");
    let envoy_dir = bundle_root.join(".envoy");
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"known": {"environment": []}}"#,
    )
    .expect("commands.json should be written");

    let assert = base_command()
        .args(["--shell", "does-not-exist"])
        .env("ENVOY_BNDL_ROOTS", scratch.path())
        .assert()
        .failure();
    let stderr = stderr_text(&assert);

    assert!(
        stderr.contains("Command 'does-not-exist' not found"),
        "stderr was:\n{stderr}"
    );
}

/// Regression test: `run_shell` previously called `prepare_env` unconditionally
/// for any COMMAND, including a raw executable path with no `--env` override.
/// `run_command`'s equivalent case skips `prepare_env` entirely and inherits
/// the system env directly -- `prepare_env`/`collect_env_files` require the
/// command name to be a *registered* command (raw paths never are), so
/// `--shell` on a raw path always failed with an environment-build error
/// before this was fixed to mirror `run_command`'s special case.
#[test]
fn shell_mode_with_a_raw_executable_path_inherits_the_system_environment() {
    let scratch = ScratchDir::new("envoy_shell_raw_path");

    #[cfg(windows)]
    let raw_path =
        env::var("ComSpec").unwrap_or_else(|_| String::from(r"C:\Windows\System32\cmd.exe"));
    #[cfg(not(windows))]
    let raw_path = String::from("/bin/sh");

    #[cfg(windows)]
    let shell_input = "exit\r\n";
    #[cfg(not(windows))]
    let shell_input = "exit\n";

    let assert = base_command()
        .args(["--shell", &raw_path])
        .current_dir(scratch.path())
        .write_stdin(shell_input)
        .assert()
        .success();
    let stdout = stdout_text(&assert);

    assert!(
        stdout.contains(&format!(
            "Entering shell inside {raw_path}'s resolved environment"
        )),
        "stdout was:\n{stdout}"
    );
    assert!(
        !stdout.contains("Failed to prepare environment"),
        "stdout was:\n{stdout}"
    );
}

/// Writes a bundle whose `mytool` command sets `SOURCE_MARKER` to `value`,
/// for asserting which of two candidate bundle roots actually won.
fn write_marker_bundle(bundle_root: &Path, value: &str) {
    let envoy_dir = bundle_root.join(".envoy");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"mytool": {"environment": ["mytool_env.json"]}}"#,
    )
    .expect("commands.json should be written");
    fs::write(
        envoy_dir.join("mytool_env.json"),
        format!(r#"{{"SOURCE_MARKER": "{value}"}}"#),
    )
    .expect("mytool_env.json should be written");
}

#[test]
fn override_bundle_substitutes_a_stack_discovered_bundles_root() {
    let scratch = ScratchDir::new("envoy_override_bundle_stack");
    let bundle_root = scratch.path().join("gt").join("maya");
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    write_marker_bundle(&bundle_root, "original-bundle");

    let override_root = scratch.path().join("dev_checkout");
    write_marker_bundle(&override_root, "overridden-bundle");

    let stack_path = scratch.path().join("studio.estack");
    write_stack(&stack_path, "bfd", &bundle_root);

    let assert = base_command()
        .arg("--stack")
        .arg(&stack_path)
        .arg("-o")
        .arg(format!("gt:maya={}", override_root.display()))
        .args(["--diagnose", "mytool"])
        .assert()
        .success();
    let stdout = stdout_text(&assert);

    assert!(
        stdout.contains("SOURCE_MARKER = overridden-bundle"),
        "stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains(&override_root.display().to_string()),
        "diagnose should report the substituted root, stdout was:\n{stdout}"
    );
}

#[test]
fn override_bundle_substitutes_a_root_discovered_bundles_root() {
    let scratch = ScratchDir::new("envoy_override_bundle_roots");
    let bundle_root = scratch.path().join("gt").join("maya");
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    write_marker_bundle(&bundle_root, "original-bundle");

    let override_root = scratch.path().join("dev_checkout");
    write_marker_bundle(&override_root, "overridden-bundle");

    let assert = base_command()
        .env("ENVOY_BNDL_ROOTS", scratch.path())
        .arg("--override-bundle")
        .arg(format!("gt:maya={}", override_root.display()))
        .args(["--diagnose", "mytool"])
        .assert()
        .success();
    let stdout = stdout_text(&assert);

    assert!(
        stdout.contains("SOURCE_MARKER = overridden-bundle"),
        "stdout was:\n{stdout}"
    );
}

#[test]
fn override_bundle_repeated_for_the_same_bndlid_uses_the_last_value() {
    let scratch = ScratchDir::new("envoy_override_bundle_last_wins");
    let bundle_root = scratch.path().join("gt").join("maya");
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    write_marker_bundle(&bundle_root, "original-bundle");

    let override_a = scratch.path().join("dev_a");
    write_marker_bundle(&override_a, "override-a");
    let override_b = scratch.path().join("dev_b");
    write_marker_bundle(&override_b, "override-b-wins");

    let assert = base_command()
        .env("ENVOY_BNDL_ROOTS", scratch.path())
        .arg("-o")
        .arg(format!("gt:maya={}", override_a.display()))
        .arg("-o")
        .arg(format!("gt:maya={}", override_b.display()))
        .args(["--diagnose", "mytool"])
        .assert()
        .success();
    let stdout = stdout_text(&assert);

    assert!(
        stdout.contains("SOURCE_MARKER = override-b-wins"),
        "stdout was:\n{stdout}"
    );
}

#[test]
fn override_bundle_reports_an_unmatched_bndlid() {
    let scratch = ScratchDir::new("envoy_override_bundle_unmatched");
    let bundle_root = scratch.path().join("gt").join("maya");
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    write_marker_bundle(&bundle_root, "original-bundle");
    let override_root = scratch.path().join("dev_checkout");
    write_marker_bundle(&override_root, "overridden-bundle");

    let assert = base_command()
        .env("ENVOY_BNDL_ROOTS", scratch.path())
        .arg("--override-bundle")
        .arg(format!("gt:does-not-exist={}", override_root.display()))
        .args(["--list"])
        .assert()
        .failure();
    let stderr = stderr_text(&assert);

    assert!(
        stderr.contains("'gt:does-not-exist' does not match any discovered bundle"),
        "stderr was:\n{stderr}"
    );
}

#[test]
fn override_bundle_reports_an_invalid_override_path() {
    let scratch = ScratchDir::new("envoy_override_bundle_invalid_path");
    let bundle_root = scratch.path().join("gt").join("maya");
    fs::create_dir_all(bundle_root.join(".git")).expect(".git dir should be created");
    write_marker_bundle(&bundle_root, "original-bundle");
    let not_a_bundle = scratch.path().join("just_a_folder");
    fs::create_dir_all(&not_a_bundle).expect("plain folder should be created");

    let assert = base_command()
        .env("ENVOY_BNDL_ROOTS", scratch.path())
        .arg("--override-bundle")
        .arg(format!("gt:maya={}", not_a_bundle.display()))
        .args(["--list"])
        .assert()
        .failure();
    let stderr = stderr_text(&assert);

    assert!(
        stderr.contains("override path is not a valid bundle"),
        "stderr was:\n{stderr}"
    );
}

#[test]
fn override_bundle_rejects_a_malformed_value() {
    let scratch = ScratchDir::new("envoy_override_bundle_malformed");

    let assert = base_command()
        .env("ENVOY_BNDL_ROOTS", scratch.path())
        .args(["--override-bundle", "no-equals-sign", "--list"])
        .assert()
        .failure();
    let stderr = stderr_text(&assert);

    assert!(
        stderr.contains("expected BNDLID=PATH"),
        "stderr was:\n{stderr}"
    );
}

#[test]
fn override_bundle_errors_when_no_bundles_were_discovered() {
    let scratch = ScratchDir::new("envoy_override_bundle_no_bundles");
    let envoy_dir = scratch.path().join(".envoy");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"known": {"environment": []}}"#,
    )
    .expect("commands.json should be written");

    let assert = base_command()
        .arg("--commands-file")
        .arg(envoy_dir.join("commands.json"))
        .arg("--override-bundle")
        .arg("gt:maya=C:/dev/maya")
        .args(["--list"])
        .assert()
        .failure();
    let stderr = stderr_text(&assert);

    assert!(
        stderr.contains("--override-bundle requires bundles discovered"),
        "stderr was:\n{stderr}"
    );
}

#[test]
fn diagnose_with_unknown_command_fails_with_clear_error() {
    let scratch = ScratchDir::new("envoy_diagnose_unknown");
    let envoy_dir = scratch.path().join(".envoy");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"known": {"environment": []}}"#,
    )
    .expect("commands.json should be written");

    let assert = base_command()
        .args(["--diagnose", "does_not_exist"])
        .current_dir(scratch.path())
        .assert()
        .failure();
    let stderr = stderr_text(&assert);

    assert!(
        stderr.contains("Command 'does_not_exist' not found"),
        "stderr was:\n{stderr}"
    );
}

fn raw_exit_code_args(code: u32) -> Vec<String> {
    #[cfg(windows)]
    {
        let comspec =
            env::var("ComSpec").unwrap_or_else(|_| String::from(r"C:\Windows\System32\cmd.exe"));
        vec![
            comspec,
            "/c".to_string(),
            "exit".to_string(),
            code.to_string(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("exit {code}"),
        ]
    }
}

#[test]
fn telemetry_is_off_by_default_and_writes_no_files() {
    let config_root = ScratchDir::new("envoy_telemetry_config_root");
    let drop_dir = ScratchDir::new("envoy_telemetry_drop_dir");
    let args = raw_exit_code_args(0);

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .args(&args)
        .assert()
        .success();

    // Closed-by-default: with no ENVOY_TELEMETRY_ENDPOINT resolved, nothing
    // should ever be written, even though the drop dir exists.
    assert_eq!(telemetry_files_in(drop_dir.path()).len(), 0);
}

#[test]
fn telemetry_incognito_flag_suppresses_recording_even_with_an_endpoint_configured() {
    let config_root = ScratchDir::new("envoy_telemetry_incognito");
    let drop_dir = ScratchDir::new("envoy_telemetry_drop_dir");
    let args = raw_exit_code_args(0);

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", drop_dir.path())
        .arg("--incognito")
        .args(&args)
        .assert()
        .success();

    assert_eq!(
        telemetry_files_in(drop_dir.path()).len(),
        0,
        "--incognito should suppress telemetry even with a resolvable endpoint"
    );
}

#[test]
fn telemetry_tag_flag_is_attached_to_the_recorded_event() {
    let config_root = ScratchDir::new("envoy_telemetry_tag");
    let drop_dir = ScratchDir::new("envoy_telemetry_drop_dir");
    let args = raw_exit_code_args(0);

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", drop_dir.path())
        .args(["--tag", "nightly-build"])
        .args(&args)
        .assert()
        .success();

    let files = telemetry_files_in(drop_dir.path());
    assert_eq!(files.len(), 1, "expected exactly one telemetry file");
    let contents = fs::read_to_string(&files[0]).expect("telemetry file should be readable");
    let value: serde_json::Value =
        serde_json::from_str(&contents).expect("telemetry file should be valid JSON");
    assert_eq!(value["attributes"]["envoy.tag"]["Str"], "nightly-build");
}

/// Regression test: `--tag` previously only reached the telemetry record
/// via `run_command`'s `ExecutionOptions`, so it silently did nothing for
/// any built-in branch of `run_cli` (`--list-configs`, `--docs`, `--list`,
/// etc.) even though those branches still record their own
/// `envoy.command.run` event. `--list-configs` is used here as a
/// representative built-in that returns before bundle/registry resolution.
#[test]
fn telemetry_tag_flag_is_attached_for_a_built_in_command_not_just_managed_commands() {
    let config_root = ScratchDir::new("envoy_telemetry_tag_builtin");
    let drop_dir = ScratchDir::new("envoy_telemetry_drop_dir");

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", drop_dir.path())
        .args(["--tag", "nightly-build", "--list-configs"])
        .assert()
        .success();

    let files = telemetry_files_in(drop_dir.path());
    assert_eq!(files.len(), 1, "expected exactly one telemetry file");
    let contents = fs::read_to_string(&files[0]).expect("telemetry file should be readable");
    let value: serde_json::Value =
        serde_json::from_str(&contents).expect("telemetry file should be valid JSON");
    assert_eq!(
        value["attributes"]["envoy.command.kind"]["Str"],
        "list_configs"
    );
    assert_eq!(value["attributes"]["envoy.tag"]["Str"], "nightly-build");
}

#[test]
fn telemetry_tag_flag_is_truncated_to_the_documented_max_length() {
    let config_root = ScratchDir::new("envoy_telemetry_tag_overlong");
    let drop_dir = ScratchDir::new("envoy_telemetry_drop_dir");
    let overlong_tag = "a".repeat(500);
    let args = raw_exit_code_args(0);

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", drop_dir.path())
        .args(["--tag", &overlong_tag])
        .args(&args)
        .assert()
        .success();

    let files = telemetry_files_in(drop_dir.path());
    assert_eq!(files.len(), 1, "expected exactly one telemetry file");
    let contents = fs::read_to_string(&files[0]).expect("telemetry file should be readable");
    let value: serde_json::Value =
        serde_json::from_str(&contents).expect("telemetry file should be valid JSON");
    let recorded_tag = value["attributes"]["envoy.tag"]["Str"]
        .as_str()
        .expect("envoy.tag should be a string attribute");
    assert_eq!(recorded_tag.chars().count(), 200);
    assert_eq!(recorded_tag, "a".repeat(200));
}

#[test]
fn telemetry_file_drop_records_a_successful_raw_executable_command() {
    let config_root = ScratchDir::new("envoy_telemetry_config_root");
    let drop_dir = ScratchDir::new("envoy_telemetry_drop_dir");
    let args = raw_exit_code_args(0);

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", drop_dir.path())
        .args(&args)
        .assert()
        .success();

    let files = telemetry_files_in(drop_dir.path());
    assert_eq!(files.len(), 1, "expected exactly one telemetry file");
    let contents = fs::read_to_string(&files[0]).expect("telemetry file should be readable");
    let value: serde_json::Value =
        serde_json::from_str(&contents).expect("telemetry file should be valid JSON");
    assert_eq!(value["name"], "envoy.command.run");
    assert_eq!(
        value["attributes"]["envoy.command.kind"]["Str"],
        "raw_executable"
    );
    assert_eq!(value["attributes"]["envoy.success"]["Bool"], true);
    assert_eq!(value["attributes"]["envoy.exit_code"]["Int"], 0);
}

#[test]
fn telemetry_records_a_nonzero_exit_managed_command() {
    let config_root = ScratchDir::new("envoy_telemetry_config_root");
    let drop_dir = ScratchDir::new("envoy_telemetry_drop_dir");
    let args = raw_exit_code_args(7);

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", drop_dir.path())
        .args(&args)
        .assert()
        .code(7);

    let files = telemetry_files_in(drop_dir.path());
    assert_eq!(files.len(), 1, "expected exactly one telemetry file");
    let contents = fs::read_to_string(&files[0]).expect("telemetry file should be readable");
    let value: serde_json::Value =
        serde_json::from_str(&contents).expect("telemetry file should be valid JSON");
    assert_eq!(value["attributes"]["envoy.success"]["Bool"], false);
    assert_eq!(value["attributes"]["envoy.exit_code"]["Int"], 7);
}

#[test]
fn telemetry_records_a_missing_command_resolution_failure() {
    let scratch = ScratchDir::new("envoy_telemetry_missing_command");
    let envoy_dir = scratch.path().join(".envoy");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"known": {"environment": []}}"#,
    )
    .expect("commands.json should be written");
    let config_root = ScratchDir::new("envoy_telemetry_config_root");
    let drop_dir = ScratchDir::new("envoy_telemetry_drop_dir");

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", drop_dir.path())
        .arg("missing_command")
        .current_dir(scratch.path())
        .assert()
        .failure();

    let files = telemetry_files_in(drop_dir.path());
    assert_eq!(files.len(), 1, "expected exactly one telemetry file");
    let contents = fs::read_to_string(&files[0]).expect("telemetry file should be readable");
    let value: serde_json::Value =
        serde_json::from_str(&contents).expect("telemetry file should be valid JSON");
    assert_eq!(
        value["attributes"]["envoy.error_category"]["Str"],
        "command_not_found"
    );
}

#[test]
fn telemetry_records_built_in_list_and_diagnose_commands() {
    let scratch = ScratchDir::new("envoy_telemetry_builtins");
    let envoy_dir = scratch.path().join(".envoy");
    fs::create_dir_all(&envoy_dir).expect(".envoy dir should be created");
    fs::write(
        envoy_dir.join("commands.json"),
        r#"{"known": {"environment": []}}"#,
    )
    .expect("commands.json should be written");
    let config_root = ScratchDir::new("envoy_telemetry_config_root");
    let drop_dir = ScratchDir::new("envoy_telemetry_drop_dir");

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", drop_dir.path())
        .arg("--list")
        .current_dir(scratch.path())
        .assert()
        .success();

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", drop_dir.path())
        .arg("--diagnose")
        .current_dir(scratch.path())
        .assert()
        .success();

    let files = telemetry_files_in(drop_dir.path());
    assert_eq!(
        files.len(),
        2,
        "expected one telemetry file per built-in invocation"
    );

    let kinds: Vec<String> = files
        .iter()
        .map(|path| {
            let contents = fs::read_to_string(path).expect("telemetry file should be readable");
            let value: serde_json::Value =
                serde_json::from_str(&contents).expect("telemetry file should be valid JSON");
            value["attributes"]["envoy.command.kind"]["Str"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert!(kinds.contains(&"list".to_string()), "kinds were: {kinds:?}");
    assert!(
        kinds.contains(&"diagnose".to_string()),
        "kinds were: {kinds:?}"
    );
}

#[test]
fn telemetry_unreachable_destination_never_changes_command_behavior_or_exit_code() {
    let config_root = ScratchDir::new("envoy_telemetry_config_root");
    let blocking_file = config_root.path().join("blocked-by-a-file");
    fs::write(&blocking_file, b"not a directory").expect("blocking file should be written");
    let unreachable_dir = blocking_file.join("telemetry-drop");

    let args = raw_exit_code_args(3);

    // The command's own exit code must be completely unaffected by an
    // unreachable telemetry destination.
    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", &unreachable_dir)
        .args(&args)
        .assert()
        .code(3);

    // The failed delivery should have been spooled rather than silently
    // discarded. Filter to `.json` record files, ignoring the sidecar
    // `.lock` file the spool also creates in the same directory.
    let spool_dir = config_root.path().join("telemetry").join("spool");
    let spooled = telemetry_files_in(&spool_dir).len();
    assert_eq!(spooled, 1, "expected the failed delivery to be spooled");
}

#[test]
fn telemetry_redacts_secret_looking_arguments_before_export() {
    let config_root = ScratchDir::new("envoy_telemetry_config_root");
    let drop_dir = ScratchDir::new("envoy_telemetry_drop_dir");
    let secret = "super-secret-token-value-123";

    #[cfg(windows)]
    let args: Vec<String> = {
        let comspec =
            env::var("ComSpec").unwrap_or_else(|_| String::from(r"C:\Windows\System32\cmd.exe"));
        vec![
            comspec,
            "/c".to_string(),
            "exit".to_string(),
            "0".to_string(),
        ]
    };
    #[cfg(not(windows))]
    let args: Vec<String> = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "exit 0".to_string(),
    ];

    base_command()
        .env("ENVOY_CONFIG_ROOT", config_root.path())
        .env("ENVOY_TELEMETRY_ENDPOINT", drop_dir.path())
        .args(&args)
        .arg("--token")
        .arg(secret)
        .assert()
        .success();

    let files = telemetry_files_in(drop_dir.path());
    assert_eq!(files.len(), 1, "expected exactly one telemetry file");
    let contents = fs::read_to_string(&files[0]).expect("telemetry file should be readable");
    assert!(
        !contents.contains(secret),
        "telemetry payload must never contain the raw secret value:\n{contents}"
    );
    assert!(contents.contains("REDACTED"));
}
