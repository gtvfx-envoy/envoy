//! `envoy.command.run` attribute schema.
//!
//! Defines the canonical attribute set and schema-version constant for the
//! root span/event envoy emits for every parsed invocation, plus a builder
//! that assembles it (applying redaction) from whatever context is
//! available at each call site in envoy-cli.
//!
//! ## Version-skew policy
//!
//! Attributes are additive by default and forward/backward compatible for
//! free: an older Collector/Tempo/dashboard simply stores or ignores
//! attributes it doesn't recognize, and a newer dashboard tolerates older
//! clients missing newer attributes. [`SCHEMA_VERSION`] is bumped only on a
//! breaking change (an attribute rename, removal, or semantic change) --
//! additive changes never bump it. Dashboard panels/TraceQL queries should
//! be written defensively (tolerate a missing attribute) rather than
//! assuming every span has every field.

use std::collections::HashMap;

use super::redact::redact_argv;
use super::TelemetryValue;

/// Event/span name for the root command-run record.
pub const COMMAND_RUN_EVENT_NAME: &str = "envoy.command.run";

/// Current schema version. Bump only for a breaking change; see the
/// version-skew policy in the module docs.
pub const SCHEMA_VERSION: i64 = 1;

// Attribute key constants -- kept as constants (rather than inline string
// literals scattered across envoy-cli) so a future rename only has to
// change one place, and so dashboard queries/docs can reference the same
// names.
pub const ATTR_SCHEMA_VERSION: &str = "envoy.schema_version";
pub const ATTR_STACK_NAME: &str = "envoy.stack.name";
pub const ATTR_STACK_NAMESPACE: &str = "envoy.stack.namespace";
pub const ATTR_STACK_REGISTRY_VERSION: &str = "envoy.stack.registry_version";
pub const ATTR_TEAM: &str = "envoy.team";
pub const ATTR_BUNDLE_ID: &str = "envoy.bundle.id";
pub const ATTR_ENVOY_VERSION: &str = "envoy.version";
pub const ATTR_INSTALLATION_ID: &str = "envoy.installation_id";
pub const ATTR_COMMAND_KIND: &str = "envoy.command.kind";
pub const ATTR_COMMAND_NAME: &str = "envoy.command.name";
pub const ATTR_SUCCESS: &str = "envoy.success";
pub const ATTR_EXIT_CODE: &str = "envoy.exit_code";
pub const ATTR_DURATION_MS: &str = "envoy.duration_ms";
pub const ATTR_CLI_ARGV_JSON: &str = "envoy.cli.argv_json";
pub const ATTR_CLI_ARGV_DISPLAY: &str = "envoy.cli.argv_display";
pub const ATTR_COMMAND_ARGV_JSON: &str = "envoy.command.argv_json";
pub const ATTR_COMMAND_ARGV_DISPLAY: &str = "envoy.command.argv_display";
pub const ATTR_TRANSPORT: &str = "envoy.telemetry.transport";
pub const ATTR_DELIVERED_VIA_RETRY: &str = "envoy.telemetry.delivered_via_retry";
pub const ATTR_ERROR_CATEGORY: &str = "envoy.error_category";
pub const ATTR_TAG: &str = "envoy.tag";

/// Prefix for indexed CLI-argument attributes (`envoy.cli.arg.0`, `.1`, ...).
pub const ATTR_CLI_ARG_PREFIX: &str = "envoy.cli.arg.";
/// Prefix for indexed command-argument attributes (`envoy.command.arg.0`, ...).
pub const ATTR_COMMAND_ARG_PREFIX: &str = "envoy.command.arg.";

/// Broad, non-identifying error categories -- never an arbitrary error
/// message or environment value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCategory {
    CommandNotFound,
    EnvironmentBuildFailure,
    ExecutionFailure,
    ResolutionFailure,
    Validation,
}

impl ErrorCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCategory::CommandNotFound => "command_not_found",
            ErrorCategory::EnvironmentBuildFailure => "environment_build_failure",
            ErrorCategory::ExecutionFailure => "execution_failure",
            ErrorCategory::ResolutionFailure => "resolution_failure",
            ErrorCategory::Validation => "validation",
        }
    }
}

/// Which broad kind of envoy-cli invocation produced a `command.run`
/// record -- one variant per early-return branch in `run_cli`, so every
/// branch can be distinguished in the dashboard without parsing argv.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum CommandKind {
    #[default]
    ManagedCommand,
    RawExecutable,
    List,
    Info,
    Which,
    Diagnose,
    Trace,
    Docs,
    Shell,
    SetConfig,
    GetConfig,
    ListConfigs,
    SetStack,
    GetStack,
    ListStacks,
    Cache,
    Noop,
    Help,
    ResolutionFailure,
}

impl CommandKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CommandKind::ManagedCommand => "managed_command",
            CommandKind::RawExecutable => "raw_executable",
            CommandKind::List => "list",
            CommandKind::Info => "info",
            CommandKind::Which => "which",
            CommandKind::Diagnose => "diagnose",
            CommandKind::Trace => "trace",
            CommandKind::Docs => "docs",
            CommandKind::Shell => "shell",
            CommandKind::SetConfig => "set_config",
            CommandKind::GetConfig => "get_config",
            CommandKind::ListConfigs => "list_configs",
            CommandKind::SetStack => "set_stack",
            CommandKind::GetStack => "get_stack",
            CommandKind::ListStacks => "list_stacks",
            CommandKind::Cache => "cache",
            CommandKind::Noop => "noop",
            CommandKind::Help => "help",
            CommandKind::ResolutionFailure => "resolution_failure",
        }
    }
}

/// Everything known about the current invocation at the point a
/// `command.run` record is built.
///
/// Optional fields are simply omitted from the resulting attribute map
/// rather than sent as empty/null, keeping payloads small and TraceQL
/// queries simple (plain "attribute present" checks).
#[derive(Clone, Debug, Default)]
pub struct CommandRunContext {
    pub kind: CommandKind,
    pub command_name: Option<String>,
    pub stack_name: Option<String>,
    pub stack_namespace: Option<String>,
    pub stack_registry_version: Option<String>,
    pub team: Option<String>,
    pub bundle_id: Option<String>,
    pub envoy_version: Option<String>,
    pub installation_id: Option<String>,
    pub success: Option<bool>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i64>,
    /// Complete original envoy argv (as received on envoy-cli's own command
    /// line), redacted before it is ever attached to the record.
    pub cli_argv: Vec<String>,
    /// Resolved subprocess argv (alias-expanded), redacted the same way.
    pub command_argv: Vec<String>,
    /// Additional sensitive flag names from `ENVOY_TELEMETRY_REDACT_ARGS`.
    pub extra_redact_args: Vec<String>,
    pub error_category: Option<ErrorCategory>,
    /// Free-text tag from `--tag`, attached as-is (not redacted -- unlike
    /// argv, this is a value the user deliberately chose to attach).
    pub tag: Option<String>,
}

impl CommandRunContext {
    /// Assemble the final attribute map for this context, applying
    /// redaction to both argv fields. Schema version and command kind are
    /// always present; every other field is included only if set.
    pub fn build_attributes(&self) -> HashMap<String, TelemetryValue> {
        let mut attrs = HashMap::new();
        attrs.insert(
            ATTR_SCHEMA_VERSION.to_string(),
            TelemetryValue::Int(SCHEMA_VERSION),
        );
        attrs.insert(
            ATTR_COMMAND_KIND.to_string(),
            TelemetryValue::Str(self.kind.as_str().to_string()),
        );

        insert_str(&mut attrs, ATTR_COMMAND_NAME, &self.command_name);
        insert_str(&mut attrs, ATTR_STACK_NAME, &self.stack_name);
        insert_str(&mut attrs, ATTR_STACK_NAMESPACE, &self.stack_namespace);
        insert_str(
            &mut attrs,
            ATTR_STACK_REGISTRY_VERSION,
            &self.stack_registry_version,
        );
        insert_str(&mut attrs, ATTR_TEAM, &self.team);
        insert_str(&mut attrs, ATTR_BUNDLE_ID, &self.bundle_id);
        insert_str(&mut attrs, ATTR_ENVOY_VERSION, &self.envoy_version);
        insert_str(&mut attrs, ATTR_INSTALLATION_ID, &self.installation_id);
        insert_str(&mut attrs, ATTR_TAG, &self.tag);

        if let Some(value) = self.success {
            attrs.insert(ATTR_SUCCESS.to_string(), TelemetryValue::Bool(value));
        }
        if let Some(value) = self.exit_code {
            attrs.insert(
                ATTR_EXIT_CODE.to_string(),
                TelemetryValue::Int(value as i64),
            );
        }
        if let Some(value) = self.duration_ms {
            attrs.insert(ATTR_DURATION_MS.to_string(), TelemetryValue::Int(value));
        }
        if let Some(category) = self.error_category {
            attrs.insert(
                ATTR_ERROR_CATEGORY.to_string(),
                TelemetryValue::Str(category.as_str().to_string()),
            );
        }

        if !self.cli_argv.is_empty() {
            let redacted = redact_argv(&self.cli_argv, &self.extra_redact_args);
            insert_argv_attributes(
                &mut attrs,
                &redacted,
                ATTR_CLI_ARGV_JSON,
                ATTR_CLI_ARGV_DISPLAY,
                ATTR_CLI_ARG_PREFIX,
            );
        }
        if !self.command_argv.is_empty() {
            let redacted = redact_argv(&self.command_argv, &self.extra_redact_args);
            insert_argv_attributes(
                &mut attrs,
                &redacted,
                ATTR_COMMAND_ARGV_JSON,
                ATTR_COMMAND_ARGV_DISPLAY,
                ATTR_COMMAND_ARG_PREFIX,
            );
        }

        attrs
    }
}

fn insert_str(attrs: &mut HashMap<String, TelemetryValue>, key: &str, value: &Option<String>) {
    if let Some(value) = value {
        attrs.insert(key.to_string(), TelemetryValue::Str(value.clone()));
    }
}

fn insert_argv_attributes(
    attrs: &mut HashMap<String, TelemetryValue>,
    redacted_argv: &[String],
    json_key: &str,
    display_key: &str,
    index_prefix: &str,
) {
    let json = serde_json::to_string(redacted_argv).unwrap_or_default();
    attrs.insert(json_key.to_string(), TelemetryValue::Str(json));
    attrs.insert(
        display_key.to_string(),
        TelemetryValue::Str(display_argv(redacted_argv)),
    );
    for (index, value) in redacted_argv.iter().enumerate() {
        attrs.insert(
            format!("{index_prefix}{index}"),
            TelemetryValue::Str(value.clone()),
        );
    }
}

/// Render `argv` as a shell-displayable string, quoting values containing
/// whitespace or that are empty.
fn display_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|value| {
            if value.is_empty() || value.contains(' ') {
                format!("\"{}\"", value.replace('"', "\\\""))
            } else {
                value.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Stamp delivery metadata onto an already-built attribute map: which
/// transport delivered the event, and whether delivery happened on the
/// first attempt or via a later spool-retry flush. Applied by the delivery
/// layer (see [`crate::telemetry`]'s dispatch helpers) right before/after
/// the record is actually sent, since that is the earliest point this is
/// knowable.
pub fn stamp_delivery_metadata(
    attrs: &mut HashMap<String, TelemetryValue>,
    transport: &str,
    delivered_via_retry: bool,
) {
    attrs.insert(
        ATTR_TRANSPORT.to_string(),
        TelemetryValue::Str(transport.to_string()),
    );
    attrs.insert(
        ATTR_DELIVERED_VIA_RETRY.to_string(),
        TelemetryValue::Bool(delivered_via_retry),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_includes_schema_version_and_command_kind() {
        let context = CommandRunContext {
            kind: CommandKind::List,
            ..Default::default()
        };
        let attrs = context.build_attributes();

        assert_eq!(
            attrs.get(ATTR_SCHEMA_VERSION),
            Some(&TelemetryValue::Int(SCHEMA_VERSION))
        );
        assert_eq!(
            attrs.get(ATTR_COMMAND_KIND),
            Some(&TelemetryValue::Str("list".to_string()))
        );
    }

    #[test]
    fn omits_unset_optional_fields() {
        let context = CommandRunContext::default();
        let attrs = context.build_attributes();

        assert!(!attrs.contains_key(ATTR_STACK_NAME));
        assert!(!attrs.contains_key(ATTR_TEAM));
        assert!(!attrs.contains_key(ATTR_EXIT_CODE));
        assert!(!attrs.contains_key(ATTR_ERROR_CATEGORY));
    }

    #[test]
    fn includes_set_optional_fields() {
        let context = CommandRunContext {
            kind: CommandKind::ManagedCommand,
            command_name: Some("unreal".to_string()),
            stack_name: Some("gt".to_string()),
            team: Some("bfd".to_string()),
            success: Some(true),
            exit_code: Some(0),
            duration_ms: Some(1500),
            ..Default::default()
        };
        let attrs = context.build_attributes();

        assert_eq!(
            attrs.get(ATTR_COMMAND_NAME),
            Some(&TelemetryValue::Str("unreal".to_string()))
        );
        assert_eq!(attrs.get(ATTR_SUCCESS), Some(&TelemetryValue::Bool(true)));
        assert_eq!(attrs.get(ATTR_EXIT_CODE), Some(&TelemetryValue::Int(0)));
        assert_eq!(
            attrs.get(ATTR_DURATION_MS),
            Some(&TelemetryValue::Int(1500))
        );
    }

    #[test]
    fn tag_is_attached_as_is_when_set_and_omitted_when_unset() {
        let tagged = CommandRunContext {
            tag: Some("nightly-build".to_string()),
            ..Default::default()
        };
        assert_eq!(
            tagged.build_attributes().get(ATTR_TAG),
            Some(&TelemetryValue::Str("nightly-build".to_string()))
        );

        let untagged = CommandRunContext::default();
        assert!(!untagged.build_attributes().contains_key(ATTR_TAG));
    }

    #[test]
    fn redacts_and_indexes_cli_argv() {
        let context = CommandRunContext {
            cli_argv: vec![
                "--token".to_string(),
                "secretvalue".to_string(),
                "unreal".to_string(),
            ],
            ..Default::default()
        };
        let attrs = context.build_attributes();

        assert_eq!(
            attrs.get(&format!("{ATTR_CLI_ARG_PREFIX}0")),
            Some(&TelemetryValue::Str("--token".to_string()))
        );
        assert_eq!(
            attrs.get(&format!("{ATTR_CLI_ARG_PREFIX}1")),
            Some(&TelemetryValue::Str("***REDACTED***".to_string()))
        );
        assert_eq!(
            attrs.get(&format!("{ATTR_CLI_ARG_PREFIX}2")),
            Some(&TelemetryValue::Str("unreal".to_string()))
        );

        let json = match attrs.get(ATTR_CLI_ARGV_JSON) {
            Some(TelemetryValue::Str(value)) => value.clone(),
            other => panic!("expected string attribute, got {other:?}"),
        };
        assert!(!json.contains("secretvalue"));

        let display = match attrs.get(ATTR_CLI_ARGV_DISPLAY) {
            Some(TelemetryValue::Str(value)) => value.clone(),
            other => panic!("expected string attribute, got {other:?}"),
        };
        assert!(!display.contains("secretvalue"));
    }

    #[test]
    fn omits_argv_attributes_when_argv_is_empty() {
        let context = CommandRunContext::default();
        let attrs = context.build_attributes();

        assert!(!attrs.contains_key(ATTR_CLI_ARGV_JSON));
        assert!(!attrs.contains_key(ATTR_COMMAND_ARGV_JSON));
    }

    #[test]
    fn stamp_delivery_metadata_sets_both_attributes() {
        let mut attrs = HashMap::new();
        stamp_delivery_metadata(&mut attrs, "file-drop", true);

        assert_eq!(
            attrs.get(ATTR_TRANSPORT),
            Some(&TelemetryValue::Str("file-drop".to_string()))
        );
        assert_eq!(
            attrs.get(ATTR_DELIVERED_VIA_RETRY),
            Some(&TelemetryValue::Bool(true))
        );
    }
}
