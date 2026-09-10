use clap::{ArgAction, CommandFactory, Parser};

const LEGACY_ALIAS_HELP: &str = "Legacy compatibility aliases: -cf, -sc, -gc, -lc, -ic";

/// Maximum length, in Unicode scalar values (not bytes), for a `--tag`
/// value before [`crate::app`] deterministically truncates it.
///
/// Telemetry is best-effort by design and never affects a command's own
/// exit code (see `CommandRunEmission::emit_and_return`'s `--incognito`
/// short-circuit) -- an overlong tag is capped rather than rejected, so a
/// telemetry-metadata mistake can never fail an otherwise-successful
/// invocation. This also keeps the on-disk/exported JSON payload bounded
/// and avoids exceeding attribute-value limits some OTLP backends enforce.
pub(crate) const MAX_TAG_LENGTH: usize = 200;

#[derive(Debug, Parser)]
#[command(
    name = "envoy",
    about = "Envoy: Cross-platform environment orchestration for applications",
    version = env!("ENVOY_VERSION"),
    after_help = LEGACY_ALIAS_HELP
)]
pub(crate) struct Cli {
    #[arg(
        long,
        value_name = "BUNDLE",
        num_args = 0..=1,
        default_missing_value = "",
        help = "Open the envoy documentation in the default browser. Pass a \
BUNDLE ID to open that bundle's own docs/index.html or README.md instead."
    )]
    pub docs: Option<String>,

    #[arg(long, help = "List all available commands")]
    pub list: bool,

    #[arg(
        long,
        value_name = "COMMAND",
        help = "Show detailed information about a command"
    )]
    pub info: Option<String>,

    #[arg(
        long,
        value_name = "COMMAND",
        help = "Show the resolved executable path for a command"
    )]
    pub which: Option<String>,

    #[arg(
        long = "commands-file",
        value_name = "PATH",
        help = "Path to commands.json file (auto-detected if not specified)"
    )]
    pub commands_file: Option<String>,

    #[arg(
        long = "stack",
        short = 's',
        value_name = "NAME_OR_PATH",
        help = "Named Stack or strict YAML .estack path"
    )]
    pub stack: Option<String>,

    #[arg(
        long = "set-stack",
        value_name = "NAME_OR_PATH",
        help = "Validate and persist NAME_OR_PATH as the default stack, then exit."
    )]
    pub set_stack: Option<String>,

    #[arg(
        long = "get-stack",
        help = "Print the current effective stack (honoring the same \
ENVOY_STACK / user-config / ENVOY_STACK_CONTEXT precedence as normal \
resolution) and exit."
    )]
    pub get_stack: bool,

    #[arg(
        long = "list-stacks",
        help = "List all named stacks discovered via ENVOY_STACK_ROOTS and exit."
    )]
    pub list_stacks: bool,

    #[arg(
        long = "override-bundle",
        short = 'o',
        value_name = "BNDLID=PATH",
        action = ArgAction::Append,
        help = "Substitute a local checkout for a bundle a resolved Stack or \
ENVOY_BNDL_ROOTS discovery would otherwise use. Repeatable; the last value \
wins for a given BNDLID."
    )]
    pub override_bundle: Vec<String>,

    #[arg(
        long = "set-config",
        value_name = "KEY=VALUE",
        help = "Set a user config value and save it. Use KEY= to clear a setting."
    )]
    pub set_config: Option<String>,

    #[arg(
        long = "get-config",
        value_name = "KEY",
        num_args = 0..=1,
        default_missing_value = "",
        help = "Print one or all user config values and exit. Omit KEY to print all settings."
    )]
    pub get_config: Option<String>,

    #[arg(
        long = "list-configs",
        help = "List all known configurable settings with their descriptions and exit."
    )]
    pub list_configs: bool,

    #[arg(
        long = "ignore-config",
        help = "Bypass the user config for this invocation."
    )]
    pub ignore_config: bool,

    #[arg(
        long,
        short = 'e',
        value_name = "ENV_COMMAND",
        help = "Run the target command inside a different command's environment."
    )]
    pub env: Option<String>,

    // Help text's "200 characters" must stay in sync with MAX_TAG_LENGTH
    // above -- clap's `help` needs a `'static str`, so it can't be built
    // from the const directly without an extra macro dependency.
    #[arg(
        long,
        value_name = "TAG",
        help = "Attach a free-text tag to this invocation's telemetry record, \
if telemetry is enabled. Truncated to 200 characters."
    )]
    pub tag: Option<String>,

    #[arg(
        long,
        help = "Disable telemetry for this invocation only, regardless of the \
usual env/config-driven opt-in."
    )]
    pub incognito: bool,

    #[arg(
        long,
        help = "Drop into an interactive shell (cmd.exe/$SHELL) inside COMMAND's \
resolved environment for inspection, instead of running COMMAND itself."
    )]
    pub shell: bool,

    #[arg(long, short = 'v', help = "Enable verbose logging")]
    pub verbose: bool,

    #[arg(
        long = "inherit-env",
        short = 'i',
        help = "Inherit the full system environment"
    )]
    pub inherit_env: bool,

    #[arg(
        long,
        value_name = "VAR",
        help = "Show how VAR is mutated through env file processing for COMMAND."
    )]
    pub trace: Option<String>,

    #[arg(
        long,
        value_name = "COMMAND",
        num_args = 0..=1,
        default_missing_value = "",
        help = "Show diagnostics: stack, discovered bundles/commands, team \
context, bundle cache status, VCS detection, and bundle-root reachability. \
Pass a COMMAND to also include its full environment resolution trace."
    )]
    pub diagnose: Option<String>,

    #[arg(help = "Command to execute")]
    pub command: Option<String>,

    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        help = "Arguments to pass to the command"
    )]
    pub args: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueExpectation {
    Required,
    Optional,
}

pub(crate) fn parse(argv: &[String]) -> Result<Cli, clap::Error> {
    let normalized = normalize_argv(argv);
    let canonical = canonicalize_legacy_aliases(&normalized);
    let mut full_argv = Vec::with_capacity(canonical.len() + 1);
    full_argv.push(String::from("envoy"));
    full_argv.extend(canonical);
    Cli::try_parse_from(full_argv)
}

pub(crate) fn print_help() -> Result<(), clap::Error> {
    let mut command = Cli::command();
    command.print_help()?;
    println!();
    Ok(())
}

pub(crate) fn normalize_argv(argv: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut command_seen = false;

    for token in argv {
        if command_seen {
            result.push(token.clone());
            continue;
        }

        if token.starts_with('-') && token.contains('=') {
            let mut parts = token.splitn(2, '=');
            let flag = parts.next().unwrap_or_default();
            let value = parts.next().unwrap_or_default();
            result.push(flag.to_string());
            result.push(value.to_string());
        } else {
            if !token.starts_with('-') {
                command_seen = true;
            }
            result.push(token.clone());
        }
    }

    result
}

fn canonicalize_legacy_aliases(argv: &[String]) -> Vec<String> {
    let mut result = Vec::with_capacity(argv.len());
    let mut command_seen = false;
    let mut expecting_value = None;

    for token in argv {
        if command_seen {
            result.push(token.clone());
            continue;
        }

        if let Some(expectation) = expecting_value {
            match expectation {
                ValueExpectation::Required => {
                    result.push(token.clone());
                    expecting_value = None;
                    continue;
                }
                ValueExpectation::Optional if !token.starts_with('-') => {
                    result.push(token.clone());
                    expecting_value = None;
                    continue;
                }
                ValueExpectation::Optional => {
                    expecting_value = None;
                }
            }
        }

        if token == "--" {
            command_seen = true;
            result.push(token.clone());
            continue;
        }

        if let Some((canonical, expectation, attached_value)) = legacy_alias(token) {
            result.push(canonical.to_string());
            if let Some(value) = attached_value {
                result.push(value.to_string());
            } else {
                expecting_value = expectation;
            }
            continue;
        }

        result.push(token.clone());

        if let Some(expectation) = option_value_expectation(token) {
            expecting_value = Some(expectation);
        } else if !token.starts_with('-') {
            command_seen = true;
        }
    }

    result
}

fn legacy_alias(token: &str) -> Option<(&'static str, Option<ValueExpectation>, Option<&str>)> {
    const ALIASES: [(&str, &str, Option<ValueExpectation>); 5] = [
        ("-cf", "--commands-file", Some(ValueExpectation::Required)),
        ("-sc", "--set-config", Some(ValueExpectation::Required)),
        ("-gc", "--get-config", Some(ValueExpectation::Optional)),
        ("-lc", "--list-configs", None),
        ("-ic", "--ignore-config", None),
    ];

    for (alias, canonical, expectation) in ALIASES {
        if token == alias {
            return Some((canonical, expectation, None));
        }

        if let Some(value) = token
            .strip_prefix(alias)
            .and_then(|tail| tail.strip_prefix('='))
        {
            return Some((canonical, None, Some(value)));
        }
    }

    None
}

fn option_value_expectation(token: &str) -> Option<ValueExpectation> {
    match token {
        "--info" | "--which" | "--commands-file" | "--stack" | "-s" | "--set-config" | "--env"
        | "-e" | "--trace" | "--override-bundle" | "-o" => Some(ValueExpectation::Required),
        "--get-config" | "--diagnose" | "--docs" => Some(ValueExpectation::Optional),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_legacy_aliases, normalize_argv, MAX_TAG_LENGTH};

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    /// The `--tag` flag's help text hardcodes "200 characters" (clap's
    /// `help` needs a `'static str`, so it can't reference the const
    /// directly) -- this pins `MAX_TAG_LENGTH` itself so a future change to
    /// one is never silently missed in the other.
    #[test]
    fn max_tag_length_matches_the_value_documented_in_help_text() {
        assert_eq!(MAX_TAG_LENGTH, 200);
    }

    #[test]
    fn normalize_argv_expands_short_and_long_equals_forms() {
        let input = strings(&["-e=python", "--stack=studio", "maya"]);
        let expected = strings(&["-e", "python", "--stack", "studio", "maya"]);

        assert_eq!(normalize_argv(&input), expected);
    }

    #[test]
    fn normalize_argv_stops_at_first_non_option_token() {
        let input = strings(&["maya", "--trace=UE_PATH", "-sc=stack=studio"]);

        assert_eq!(normalize_argv(&input), input);
    }

    #[test]
    fn normalize_argv_preserves_equals_in_option_values() {
        let input = strings(&["-sc=stack=C:\\path=with=equals.estack"]);
        let expected = strings(&["-sc", "stack=C:\\path=with=equals.estack"]);

        assert_eq!(normalize_argv(&input), expected);
    }

    #[test]
    fn normalize_argv_does_not_expand_child_process_args() {
        let input = strings(&["python", "-c", "x=1", "--flag=value"]);

        assert_eq!(normalize_argv(&input), input);
    }

    #[test]
    fn canonicalize_legacy_aliases_translates_multi_char_aliases() {
        let input = strings(&["-cf", "commands.json", "-lc"]);
        let expected = strings(&["--commands-file", "commands.json", "--list-configs"]);

        assert_eq!(canonicalize_legacy_aliases(&input), expected);
    }

    #[test]
    fn canonicalize_legacy_aliases_understands_option_values_before_command() {
        let input = strings(&["-e", "python", "-s", "studio", "maya", "-gc"]);
        let expected = strings(&["-e", "python", "-s", "studio", "maya", "-gc"]);

        assert_eq!(canonicalize_legacy_aliases(&input), expected);
    }

    /// Regression guard: `--override-bundle`/`-o` take a required value.
    /// Without `option_value_expectation` recognizing them, the loop would
    /// treat the `BNDLID=PATH` value as the very next "command" token,
    /// corrupting argument parsing for every invocation using this flag.
    #[test]
    fn canonicalize_legacy_aliases_treats_override_bundle_value_as_an_option_value() {
        let input = strings(&["--override-bundle", "gt:maya=C:\\dev\\maya", "maya"]);

        assert_eq!(canonicalize_legacy_aliases(&input), input);
    }

    #[test]
    fn canonicalize_legacy_aliases_treats_override_bundle_short_flag_as_an_option_value() {
        let input = strings(&[
            "-o",
            "gt:maya=C:\\dev\\maya",
            "-o",
            "gt:unreal=/dev/unreal",
            "maya",
        ]);

        assert_eq!(canonicalize_legacy_aliases(&input), input);
    }

    #[test]
    fn normalize_argv_expands_override_bundle_equals_form_without_mangling_the_value() {
        let input = strings(&["--override-bundle=gt:maya=C:\\dev\\maya", "maya"]);
        let expected = strings(&["--override-bundle", "gt:maya=C:\\dev\\maya", "maya"]);

        assert_eq!(normalize_argv(&input), expected);
    }
}
