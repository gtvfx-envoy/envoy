//! Environment-variable loading, expansion, and subprocess preparation.
//!
//! This module ports `py/envoy/_environment.py` into `envoy-core`.
//! It implements envoy's JSON-driven environment operator engine, including:
//!
//! - `=`, `+=`, `^=`, and `?=` operator handling
//! - `${VAR}`, `${?VAR}`, and legacy `{$VAR}` expansion
//! - path normalization to the current platform
//! - closed-environment allowlist seeding
//! - special bundle/file variables
//! - trace events describing how a variable reached its final value
//!
//! The Rust port preserves the Python module's operational rules as closely as
//! possible while exposing them through idiomatic Rust types.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::error::{EnvoyError, Result};
use crate::json_util::parse_json_with_comments;

const BUNDLE_ENV_DIR: &str = ".envoy";

static CORE_ENV_VARS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "USERNAME",
        "USERPROFILE",
        "USERDOMAIN",
        "USERDOMAIN_ROAMINGPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "PUBLIC",
        "PROGRAMDATA",
        "ALLUSERSPROFILE",
        "TEMP",
        "TMP",
        "TMPDIR",
        "SystemRoot",
        "SystemDrive",
        "windir",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "CommonProgramFiles",
        "CommonProgramFiles(x86)",
        "CommonProgramW6432",
        "COMPUTERNAME",
        "OS",
        "PROCESSOR_ARCHITECTURE",
        "PROCESSOR_IDENTIFIER",
        "PROCESSOR_LEVEL",
        "PROCESSOR_REVISION",
        "NUMBER_OF_PROCESSORS",
        "COMSPEC",
        "PATHEXT",
        "TERM",
        "TERM_PROGRAM",
        "COLORTERM",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LC_MESSAGES",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ])
});

static ENVOY_ENV_VARS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "DESPATCH_CONFIG_ROOT",
        "ENVOY_ALLOWLIST",
        "ENVOY_BIN_FLOAT",
        "ENVOY_BIN_LOCK",
        "ENVOY_BNDL_PROD",
        "ENVOY_BNDL_ROOTS",
        "ENVOY_CONFIG_ROOT",
        "ENVOY_CONFIG_PROD",
        "ENVOY_PYTHON_API",
        "ENVOY_REDIRECT_CONFIG",
        "ENVOY_SHIM_TIMEOUT_MS",
        "ENVOY_STACK",
        "ENVOY_STACK_CONTEXT",
        "ENVOY_STACK_ROOTS",
        "ENVOY_TEAM",
        "ENVOY_USER",
        "ENVOY_VERSION",
        "ENVOY_BUNDLE_CACHE",
        "ENVOY_CACHE",
        "ENVOY_CACHE_MAX_IDLE_DAYS",
        "ENVOY_CACHE_MAX_SIZE",
        "ENVOY_CACHE_ROOT",
        "ENVOY_CACHE_THREADS",
        "ENVOY_DISABLE_BUNDLE_CACHE",
        "ENVOY_STUDIO_ARTIFACTS",
        "ENVOY_STUDIO_ASSETS",
        "ENVOY_STUDIO_BNDLS",
    ])
});

/// Return the core OS variables always seeded in closed-environment mode.
pub fn core_env_vars() -> &'static HashSet<&'static str> {
    &CORE_ENV_VARS
}

/// Return envoy-specific variables always seeded in closed-environment mode.
pub fn envoy_env_vars() -> &'static HashSet<&'static str> {
    &ENVOY_ENV_VARS
}

/// Trace event emitted when the traced variable appears in an allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceAllowlistEvent {
    /// Environment file that declared the allowlist entry.
    pub file_path: PathBuf,
    /// Variable name being traced.
    pub var_name: String,
    /// Whether the variable was actually written into the merged environment.
    pub seeded: bool,
    /// Value from the current process environment when seeding was attempted.
    pub os_value: String,
    /// Whether the variable was already present in `merged_env`.
    pub already_set: bool,
}

/// Trace event emitted for each env-file entry touching the traced variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceStepEvent {
    /// Environment file providing the entry.
    pub file_path: PathBuf,
    /// Variable name being traced.
    pub var_name: String,
    /// Operator used by the entry: `=`, `+=`, `^=`, or `?=`.
    pub operator: String,
    /// Raw JSON value, serialized with compact JSON formatting.
    pub raw_value: String,
    /// Value after expansion and normalization.
    pub expanded_value: String,
    /// Value before the entry was processed, or an empty string if absent.
    pub value_before: String,
    /// Value after the entry was processed.
    pub value_after: String,
    /// Whether the entry was applied.
    pub was_applied: bool,
}

/// Polymorphic trace output event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    /// Allowlist pre-pass event.
    Allowlist(TraceAllowlistEvent),
    /// Per-entry processing event.
    Step(TraceStepEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefKind {
    Required,
    Optional,
    LegacyRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VariableRef {
    start: usize,
    end: usize,
    name: String,
    kind: RefKind,
}

#[derive(Debug, Clone, PartialEq)]
enum OrderedValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<OrderedValue>),
    Object(Vec<(String, OrderedValue)>),
}

impl<'de> serde::Deserialize<'de> for OrderedValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OrderedValueVisitor;

        impl<'de> Visitor<'de> for OrderedValueVisitor {
            type Value = OrderedValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a valid JSON value")
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(OrderedValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
                Ok(OrderedValue::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
                Ok(OrderedValue::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(OrderedValue::Number)
                    .ok_or_else(|| E::custom("invalid floating-point value"))
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(OrderedValue::Null)
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(OrderedValue::Null)
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(OrderedValue::String(value.to_string()))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
                Ok(OrderedValue::String(value))
            }

            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<OrderedValue>()? {
                    values.push(value);
                }

                Ok(OrderedValue::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, OrderedValue>()? {
                    entries.push((key, value));
                }

                Ok(OrderedValue::Object(entries))
            }
        }

        deserializer.deserialize_any(OrderedValueVisitor)
    }
}

impl OrderedValue {
    fn json_dump(&self) -> String {
        match self {
            OrderedValue::Null => String::from("null"),
            OrderedValue::Bool(value) => value.to_string(),
            OrderedValue::Number(value) => value.to_string(),
            OrderedValue::String(value) => {
                serde_json::to_string(value).expect("JSON string serialization should succeed")
            }
            OrderedValue::Array(values) => {
                let contents = values
                    .iter()
                    .map(OrderedValue::json_dump)
                    .collect::<Vec<_>>()
                    .join(",");

                format!("[{contents}]")
            }
            OrderedValue::Object(entries) => {
                let contents = entries
                    .iter()
                    .map(|(key, value)| {
                        let key_json = serde_json::to_string(key)
                            .expect("JSON string serialization should succeed");
                        format!("{key_json}:{}", value.json_dump())
                    })
                    .collect::<Vec<_>>()
                    .join(",");

                format!("{{{contents}}}")
            }
        }
    }

    fn python_type_name(&self) -> &'static str {
        match self {
            OrderedValue::Null => "NoneType",
            OrderedValue::Bool(_) => "bool",
            OrderedValue::Number(number) => {
                if number.is_f64() {
                    "float"
                } else {
                    "int"
                }
            }
            OrderedValue::String(_) => "str",
            OrderedValue::Array(_) => "list",
            OrderedValue::Object(_) => "dict",
        }
    }
}

/// Environment loader and environment-construction orchestrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentManager {
    /// Whether the child process inherits the full parent environment.
    pub inherit_env: bool,
    /// Additional variables allowed through in closed mode.
    pub allowlist: HashSet<String>,
}

impl EnvironmentManager {
    /// Create a new environment manager.
    pub fn new(inherit_env: bool, allowlist: Option<HashSet<String>>) -> Self {
        Self {
            inherit_env,
            allowlist: allowlist.unwrap_or_default(),
        }
    }

    /// Expand `${VAR}`, `${?VAR}`, and legacy `{$VAR}` references.
    ///
    /// Lookup priority matches Python:
    /// 1. `special_vars`
    /// 2. `current_env`
    ///
    /// Unresolved references expand to an empty string.
    pub fn expand_env_value(
        value: &str,
        current_env: &HashMap<String, String>,
        special_vars: Option<&HashMap<String, String>>,
    ) -> String {
        let refs = scan_variable_refs(value);
        if refs.is_empty() {
            return value.to_string();
        }

        let mut expanded = String::with_capacity(value.len());
        let mut cursor = 0;

        for variable_ref in refs {
            expanded.push_str(&value[cursor..variable_ref.start]);

            let replacement = special_vars
                .and_then(|vars| vars.get(&variable_ref.name))
                .or_else(|| current_env.get(&variable_ref.name))
                .map(String::as_str)
                .unwrap_or_default();

            expanded.push_str(replacement);
            cursor = variable_ref.end;
        }

        expanded.push_str(&value[cursor..]);
        expanded
    }

    /// Normalize forward slashes to the native separator on Windows.
    pub fn normalize_path(path: &str) -> String {
        if cfg!(windows) {
            path.replace('/', "\\")
        } else {
            path.to_string()
        }
    }

    fn find_unresolved_refs(
        value: &str,
        current_env: &HashMap<String, String>,
        special_vars: Option<&HashMap<String, String>>,
    ) -> HashSet<String> {
        scan_variable_refs(value)
            .into_iter()
            .filter(|variable_ref| {
                matches!(
                    variable_ref.kind,
                    RefKind::Required | RefKind::LegacyRequired
                )
            })
            .filter(|variable_ref| {
                !special_vars.is_some_and(|vars| vars.contains_key(&variable_ref.name))
                    && !current_env.contains_key(&variable_ref.name)
            })
            .map(|variable_ref| variable_ref.name)
            .collect()
    }

    fn find_undefined_optional_refs(
        value: &str,
        current_env: &HashMap<String, String>,
        special_vars: Option<&HashMap<String, String>>,
    ) -> HashSet<String> {
        scan_variable_refs(value)
            .into_iter()
            .filter(|variable_ref| variable_ref.kind == RefKind::Optional)
            .filter(|variable_ref| {
                !special_vars.is_some_and(|vars| vars.contains_key(&variable_ref.name))
                    && !current_env.contains_key(&variable_ref.name)
            })
            .map(|variable_ref| variable_ref.name)
            .collect()
    }

    /// Process one JSON env value into a final string.
    ///
    /// Lists are joined with the platform path separator before expansion.
    fn process_env_value(
        &self,
        value: &OrderedValue,
        merged_env: &HashMap<String, String>,
        special_vars: Option<&HashMap<String, String>>,
    ) -> String {
        let path_sep = path_separator();

        let string_value = match value {
            OrderedValue::Array(items) => items
                .iter()
                .map(ordered_value_to_env_string)
                .collect::<Vec<_>>()
                .join(path_sep),
            _ => ordered_value_to_env_string(value),
        };

        let expanded = Self::expand_env_value(&string_value, merged_env, special_vars);
        Self::normalize_path(&expanded)
    }

    /// Return bundle/file-scoped special variables for one env file.
    pub fn get_special_variables(env_file_path: &Path) -> HashMap<String, String> {
        let env_file_abs = absolute_lexical_path(env_file_path);
        let current_dir = env_file_abs.parent().unwrap_or(env_file_abs.as_path());

        let mut bundle_env_dir = None;
        let mut bundle_root = None;

        for parent in current_dir.ancestors() {
            if parent.file_name() == Some(OsStr::new(BUNDLE_ENV_DIR)) {
                bundle_env_dir = Some(parent.to_path_buf());
                bundle_root = Some(parent.parent().unwrap_or(parent).to_path_buf());
                break;
            }
        }

        let bundle_root = bundle_root.unwrap_or_else(|| current_dir.to_path_buf());
        let bundle_env_dir = bundle_env_dir.unwrap_or_else(|| bundle_root.clone());

        HashMap::from([
            (
                String::from("__FILE__"),
                path_to_forward_slashes(&env_file_abs),
            ),
            (
                String::from("__BUNDLE__"),
                path_to_forward_slashes(&bundle_root),
            ),
            (
                String::from("__BUNDLE_ENV__"),
                path_to_forward_slashes(&bundle_env_dir),
            ),
            (
                String::from("__BUNDLE_NAME__"),
                bundle_root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        ])
    }

    /// Load and merge environment entries from JSON env files.
    pub fn load_env_from_files(
        &self,
        env_files: &[PathBuf],
        base_env: Option<&HashMap<String, String>>,
        trace_var: Option<&str>,
        trace_out: Option<&mut Vec<TraceEvent>>,
        allowlist_out: Option<&mut Vec<String>>,
    ) -> Result<HashMap<String, String>> {
        if env_files.is_empty() {
            return Ok(base_env.cloned().unwrap_or_default());
        }

        let path_sep = path_separator();
        let mut merged_env = base_env.cloned().unwrap_or_default();
        let mut parsed_files = Vec::new();
        let mut trace_out = trace_out;
        let mut allowlist_out = allowlist_out;

        for env_file in env_files {
            if !env_file.exists() {
                return Err(environment_build(format!(
                    "Environment file not found: {}",
                    env_file.display()
                )));
            }

            let contents = fs::read_to_string(env_file).map_err(|error| {
                environment_build(format!(
                    "Error reading environment file {}: {error}",
                    env_file.display()
                ))
            })?;
            let parsed = parse_json_with_comments::<OrderedValue>(&contents).map_err(|error| {
                environment_build(format!(
                    "Invalid JSON in environment file {}: {error}",
                    env_file.display()
                ))
            })?;

            parsed_files.push((env_file.clone(), parsed));
        }

        for (path, file_data) in &parsed_files {
            if let OrderedValue::Object(entries) = file_data {
                for allowlisted_var in object_field(entries, "environment_allowlist")
                    .and_then(as_array)
                    .into_iter()
                    .flatten()
                {
                    let var_name = ordered_value_to_env_string(allowlisted_var);
                    let already_set = merged_env.contains_key(&var_name);
                    let os_value =
                        env::var_os(&var_name).map(|value| value.to_string_lossy().into_owned());
                    let seeded = !already_set && os_value.is_some();

                    if seeded {
                        merged_env.insert(var_name.clone(), os_value.clone().unwrap_or_default());
                    }

                    if let Some(allowlist_entries) = allowlist_out.as_deref_mut() {
                        allowlist_entries.push(var_name.clone());
                    }

                    if trace_var == Some(var_name.as_str()) {
                        if let Some(trace_events) = trace_out.as_deref_mut() {
                            trace_events.push(TraceEvent::Allowlist(TraceAllowlistEvent {
                                file_path: path.clone(),
                                var_name: var_name.clone(),
                                seeded,
                                os_value: os_value.unwrap_or_default(),
                                already_set,
                            }));
                        }
                    }
                }
            }
        }

        for (path, file_data) in parsed_files {
            let special_vars = Self::get_special_variables(&path);
            let items = env_items_from_value(&path, &file_data)?;

            for (key, mut value) in items {
                let (operator, var_name) = parse_operator(&key);

                if matches!(value, OrderedValue::Null) {
                    log_warning(&format!(
                        "Skipping '{var_name}' in {}: value is null",
                        path.display()
                    ));
                    continue;
                }

                match &mut value {
                    OrderedValue::Array(items) => {
                        let mut filtered = Vec::new();

                        for item in items.iter() {
                            if matches!(item, OrderedValue::Null) {
                                log_warning(&format!(
                                    "Skipping null item for '{var_name}' in {}",
                                    path.display()
                                ));
                                continue;
                            }

                            let item_string = ordered_value_to_env_string(item);
                            let undefined_optional = Self::find_undefined_optional_refs(
                                &item_string,
                                &merged_env,
                                Some(&special_vars),
                            );
                            if !undefined_optional.is_empty() {
                                continue;
                            }

                            let unresolved = Self::find_unresolved_refs(
                                &item_string,
                                &merged_env,
                                Some(&special_vars),
                            );
                            if !unresolved.is_empty() {
                                log_warning(&format!(
                                    "Skipping item '{}' for '{}' in {}: undefined variable(s): {}",
                                    item_string,
                                    var_name,
                                    path.display(),
                                    sorted_names(&unresolved)
                                ));
                                continue;
                            }

                            filtered.push(item.clone());
                        }

                        value = OrderedValue::Array(filtered);
                    }
                    OrderedValue::String(text) => {
                        let undefined_optional = Self::find_undefined_optional_refs(
                            text,
                            &merged_env,
                            Some(&special_vars),
                        );
                        if !undefined_optional.is_empty() {
                            continue;
                        }

                        let unresolved =
                            Self::find_unresolved_refs(text, &merged_env, Some(&special_vars));
                        if !unresolved.is_empty() {
                            log_warning(&format!(
                                "Variable '{}' in {} references undefined variable(s): {}",
                                var_name,
                                path.display(),
                                sorted_names(&unresolved)
                            ));
                        }
                    }
                    _ => {}
                }

                let processed_value =
                    self.process_env_value(&value, &merged_env, Some(&special_vars));
                let value_before = merged_env.get(&var_name).cloned().unwrap_or_default();
                let mut was_applied = true;

                match operator {
                    Operator::Default => {
                        if !merged_env.contains_key(&var_name) {
                            merged_env.insert(var_name.clone(), processed_value.clone());
                        } else {
                            was_applied = false;
                        }
                    }
                    Operator::Append => {
                        let current_value = merged_env.get(&var_name).cloned().unwrap_or_default();
                        let new_value = if current_value.is_empty() {
                            processed_value.clone()
                        } else {
                            format!("{current_value}{path_sep}{processed_value}")
                        };
                        merged_env.insert(var_name.clone(), new_value);
                    }
                    Operator::Prepend => {
                        let current_value = merged_env.get(&var_name).cloned().unwrap_or_default();
                        let new_value = if current_value.is_empty() {
                            processed_value.clone()
                        } else {
                            format!("{processed_value}{path_sep}{current_value}")
                        };
                        merged_env.insert(var_name.clone(), new_value);
                    }
                    Operator::Replace => {
                        merged_env.insert(var_name.clone(), processed_value.clone());
                    }
                }

                if trace_var == Some(var_name.as_str()) {
                    if let Some(trace_events) = trace_out.as_deref_mut() {
                        trace_events.push(TraceEvent::Step(TraceStepEvent {
                            file_path: path.clone(),
                            var_name: var_name.clone(),
                            operator: operator.as_str().to_string(),
                            raw_value: value.json_dump(),
                            expanded_value: processed_value,
                            value_before,
                            value_after: merged_env.get(&var_name).cloned().unwrap_or_default(),
                            was_applied,
                        }));
                    }
                }
            }
        }

        Ok(merged_env)
    }

    /// Prepare the final subprocess environment.
    pub fn prepare_environment(
        &self,
        env_files: &[PathBuf],
        env_overrides: Option<&HashMap<String, String>>,
        trace_var: Option<&str>,
        trace_out: Option<&mut Vec<TraceEvent>>,
    ) -> Result<HashMap<String, String>> {
        let mut result_env = if self.inherit_env {
            current_process_env()
        } else {
            let mut seeded = HashMap::new();

            for var_name in core_env_vars()
                .iter()
                .chain(envoy_env_vars().iter())
                .copied()
                .chain(self.allowlist.iter().map(String::as_str))
            {
                if let Some(value) = env::var_os(var_name) {
                    seeded.insert(var_name.to_string(), value.to_string_lossy().into_owned());
                }
            }

            seeded
        };

        let mut allowlist_additions = Vec::new();
        let file_env = self.load_env_from_files(
            env_files,
            Some(&result_env),
            trace_var,
            trace_out,
            Some(&mut allowlist_additions),
        )?;
        result_env.extend(file_env);

        if !allowlist_additions.is_empty() {
            let existing = result_env
                .get("ENVOY_ALLOWLIST")
                .cloned()
                .unwrap_or_default();
            let mut combined: BTreeSet<String> = existing
                .replace(',', ";")
                .split(';')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            combined.extend(allowlist_additions);

            result_env.insert(
                String::from("ENVOY_ALLOWLIST"),
                combined.into_iter().collect::<Vec<_>>().join(";"),
            );
        }

        if let Some(overrides) = env_overrides {
            result_env.extend(overrides.clone());
        }

        Ok(result_env)
    }

    /// Produce a full diagnostic trace of how every variable in `env_files`
    /// was resolved.
    ///
    /// Unlike [`prepare_environment`](Self::prepare_environment), which traces
    /// a single variable, this walks **all** entries across all files and emits
    /// one [`TraceEvent`] per entry plus allowlist pre-pass events. The result
    /// is suitable for diagnostic / debugging output: callers can render it as
    /// a human-readable report showing the complete resolution chain for every
    /// variable that appeared in any env file.
    pub fn diagnose_environment(
        &self,
        env_files: &[PathBuf],
        base_env: Option<&HashMap<String, String>>,
    ) -> Result<(HashMap<String, String>, Vec<TraceEvent>)> {
        if env_files.is_empty() {
            let base = base_env.cloned().unwrap_or_default();
            return Ok((base, Vec::new()));
        }

        let mut merged_env = base_env.cloned().unwrap_or_default();
        let mut parsed_files = Vec::new();
        let mut all_trace_events = Vec::new();
        let mut allowlist_additions = Vec::new();

        // Parse all files first (same error handling as load_env_from_files).
        for env_file in env_files {
            if !env_file.exists() {
                return Err(environment_build(format!(
                    "Environment file not found: {}",
                    env_file.display()
                )));
            }

            let contents = fs::read_to_string(env_file).map_err(|error| {
                environment_build(format!(
                    "Error reading environment file {}: {error}",
                    env_file.display()
                ))
            })?;
            let parsed = parse_json_with_comments::<OrderedValue>(&contents).map_err(|error| {
                environment_build(format!(
                    "Invalid JSON in environment file {}: {error}",
                    env_file.display()
                ))
            })?;

            parsed_files.push((env_file.clone(), parsed));
        }

        // Allowlist pre-pass across all files.
        for (path, file_data) in &parsed_files {
            if let OrderedValue::Object(entries) = file_data {
                for allowlisted_var in object_field(entries, "environment_allowlist")
                    .and_then(as_array)
                    .into_iter()
                    .flatten()
                {
                    let var_name = ordered_value_to_env_string(allowlisted_var);
                    let already_set = merged_env.contains_key(&var_name);
                    let os_value = env::var_os(&var_name)
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let seeded = !already_set && !os_value.is_empty();

                    if seeded {
                        merged_env.insert(var_name.clone(), os_value.clone());
                    }

                    allowlist_additions.push(var_name.clone());

                    all_trace_events.push(TraceEvent::Allowlist(TraceAllowlistEvent {
                        file_path: path.clone(),
                        var_name: var_name.clone(),
                        seeded,
                        os_value,
                        already_set,
                    }));
                }
            }
        }

        // Extend merged env with allowlist additions.
        if !allowlist_additions.is_empty() {
            let existing = merged_env
                .get("ENVOY_ALLOWLIST")
                .cloned()
                .unwrap_or_default();
            let mut combined: BTreeSet<String> = existing
                .replace(',', ";")
                .split(';')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            combined.extend(allowlist_additions);

            merged_env.insert(
                String::from("ENVOY_ALLOWLIST"),
                combined.into_iter().collect::<Vec<_>>().join(";"),
            );
        }

        // Trace every variable entry across all files using the same
        // processing pipeline as `load_env_from_files`.
        let path_sep = path_separator();
        for (path, file_data) in parsed_files {
            let special_vars = Self::get_special_variables(&path);
            let items = env_items_from_value(&path, &file_data)?;

            for (key, value) in items {
                let (operator, var_name) = parse_operator(&key);

                if matches!(value, OrderedValue::Null) {
                    continue;
                }

                // Process the value through the same pipeline as normal loading.
                let processed_value =
                    self.process_env_value(&value, &merged_env, Some(&special_vars));
                let value_before = merged_env.get(&var_name).cloned().unwrap_or_default();

                let was_applied = match operator {
                    Operator::Default => {
                        if !merged_env.contains_key(&var_name) {
                            merged_env.insert(var_name.clone(), processed_value.clone());
                            true
                        } else {
                            false
                        }
                    }
                    Operator::Append => {
                        let current = merged_env.get(&var_name).cloned().unwrap_or_default();
                        let new_val = if current.is_empty() {
                            processed_value.clone()
                        } else {
                            format!("{current}{path_sep}{processed_value}")
                        };
                        merged_env.insert(var_name.clone(), new_val);
                        true
                    }
                    Operator::Prepend => {
                        let current = merged_env.get(&var_name).cloned().unwrap_or_default();
                        let new_val = if current.is_empty() {
                            processed_value.clone()
                        } else {
                            format!("{processed_value}{path_sep}{current}")
                        };
                        merged_env.insert(var_name.clone(), new_val);
                        true
                    }
                    Operator::Replace => {
                        merged_env.insert(var_name.clone(), processed_value.clone());
                        true
                    }
                };

                let value_after = merged_env.get(&var_name).cloned().unwrap_or_default();

                all_trace_events.push(TraceEvent::Step(TraceStepEvent {
                    file_path: path.clone(),
                    var_name: var_name.clone(),
                    operator: operator.as_str().to_string(),
                    raw_value: value.json_dump(),
                    expanded_value: processed_value,
                    value_before,
                    value_after,
                    was_applied,
                }));
            }
        }

        Ok((merged_env, all_trace_events))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Replace,
    Append,
    Prepend,
    Default,
}

impl Operator {
    fn as_str(self) -> &'static str {
        match self {
            Operator::Replace => "=",
            Operator::Append => "+=",
            Operator::Prepend => "^=",
            Operator::Default => "?=",
        }
    }
}

fn parse_operator(key: &str) -> (Operator, String) {
    if let Some(var_name) = key.strip_prefix("?=") {
        (Operator::Default, var_name.to_string())
    } else if let Some(var_name) = key.strip_prefix("+=") {
        (Operator::Append, var_name.to_string())
    } else if let Some(var_name) = key.strip_prefix("^=") {
        (Operator::Prepend, var_name.to_string())
    } else {
        (Operator::Replace, key.to_string())
    }
}

fn path_separator() -> &'static str {
    if cfg!(windows) {
        ";"
    } else {
        ":"
    }
}

fn environment_build(message: String) -> EnvoyError {
    EnvoyError::EnvironmentBuild(message)
}

fn ordered_value_to_env_string(value: &OrderedValue) -> String {
    match value {
        OrderedValue::Null => String::from("None"),
        OrderedValue::Bool(value) => {
            if *value {
                String::from("True")
            } else {
                String::from("False")
            }
        }
        OrderedValue::Number(value) => value.to_string(),
        OrderedValue::String(value) => value.clone(),
        OrderedValue::Array(_) | OrderedValue::Object(_) => value.json_dump(),
    }
}

fn current_process_env() -> HashMap<String, String> {
    env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn object_field<'a>(entries: &'a [(String, OrderedValue)], key: &str) -> Option<&'a OrderedValue> {
    entries
        .iter()
        .find_map(|(entry_key, value)| (entry_key == key).then_some(value))
}

fn as_array(value: &OrderedValue) -> Option<&[OrderedValue]> {
    match value {
        OrderedValue::Array(items) => Some(items),
        _ => None,
    }
}

fn env_items_from_value(path: &Path, value: &OrderedValue) -> Result<Vec<(String, OrderedValue)>> {
    match value {
        OrderedValue::Array(entries) => parse_pair_entries(entries, "list", path),
        OrderedValue::Object(entries) => {
            if let Some(env_entries) = object_field(entries, "environment") {
                match env_entries {
                    OrderedValue::Object(env_map) => Ok(env_map.to_vec()),
                    OrderedValue::Array(env_list) => {
                        let unknown_keys = entries
                            .iter()
                            .map(|(key, _)| key.as_str())
                            .filter(|key| *key != "environment" && *key != "environment_allowlist")
                            .collect::<Vec<_>>();
                        if !unknown_keys.is_empty() {
                            log_warning(&format!(
                                "Unknown keys in structured env file {}: {}",
                                path.display(),
                                unknown_keys.join(", ")
                            ));
                        }

                        parse_pair_entries(env_list, "environment", path)
                    }
                    _ => Err(environment_build(format!(
                        "\"environment\" in {} must be a list or dict, got {}",
                        path.display(),
                        env_entries.python_type_name()
                    ))),
                }
            } else {
                Ok(entries.to_vec())
            }
        }
        _ => Err(environment_build(format!(
            "Environment file must contain a JSON object or array: {}",
            path.display()
        ))),
    }
}

fn parse_pair_entries(
    entries: &[OrderedValue],
    context: &str,
    path: &Path,
) -> Result<Vec<(String, OrderedValue)>> {
    let mut items = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        let OrderedValue::Array(pair) = entry else {
            return Err(environment_build(format!(
                "{context} entry {index} in {} must be a [key, value] pair, got: {}",
                path.display(),
                entry.json_dump()
            )));
        };

        if pair.len() != 2 {
            return Err(environment_build(format!(
                "{context} entry {index} in {} must be a [key, value] pair, got: {}",
                path.display(),
                entry.json_dump()
            )));
        }

        let OrderedValue::String(key) = &pair[0] else {
            return Err(environment_build(format!(
                "{context} entry {index} in {} must be a [key, value] pair, got: {}",
                path.display(),
                entry.json_dump()
            )));
        };

        items.push((key.clone(), pair[1].clone()));
    }

    Ok(items)
}

fn sorted_names(values: &HashSet<String>) -> String {
    let mut names = values.iter().cloned().collect::<Vec<_>>();
    names.sort();
    names.join(", ")
}

fn log_warning(message: &str) {
    eprintln!("warning: {message}");
}

fn absolute_lexical_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    lexical_normalize(&absolute)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                } else {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    normalized
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn scan_variable_refs(value: &str) -> Vec<VariableRef> {
    let bytes = value.as_bytes();
    let mut refs = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'$' && bytes.get(index + 1) == Some(&b'{') {
            if let Some(variable_ref) = parse_dollar_ref(bytes, index) {
                index = variable_ref.end;
                refs.push(variable_ref);
                continue;
            }
        } else if bytes[index] == b'{' && bytes.get(index + 1) == Some(&b'$') {
            if let Some(variable_ref) = parse_legacy_ref(bytes, index) {
                index = variable_ref.end;
                refs.push(variable_ref);
                continue;
            }
        }

        index += 1;
    }

    refs
}

fn parse_dollar_ref(bytes: &[u8], start: usize) -> Option<VariableRef> {
    let mut index = start + 2;
    let kind = if bytes.get(index) == Some(&b'?') {
        index += 1;
        RefKind::Optional
    } else {
        RefKind::Required
    };

    let name_start = index;
    if !bytes
        .get(name_start)
        .is_some_and(|byte| is_var_start(*byte))
    {
        return None;
    }

    index += 1;
    while bytes.get(index).is_some_and(|byte| is_var_continue(*byte)) {
        index += 1;
    }

    if bytes.get(index) != Some(&b'}') {
        return None;
    }

    Some(VariableRef {
        start,
        end: index + 1,
        name: std::str::from_utf8(&bytes[name_start..index])
            .expect("variable names are ASCII")
            .to_string(),
        kind,
    })
}

fn parse_legacy_ref(bytes: &[u8], start: usize) -> Option<VariableRef> {
    let mut index = start + 2;
    let name_start = index;

    if !bytes
        .get(name_start)
        .is_some_and(|byte| is_var_start(*byte))
    {
        return None;
    }

    index += 1;
    while bytes.get(index).is_some_and(|byte| is_var_continue(*byte)) {
        index += 1;
    }

    if bytes.get(index) != Some(&b'}') {
        return None;
    }

    Some(VariableRef {
        start,
        end: index + 1,
        name: std::str::from_utf8(&bytes[name_start..index])
            .expect("variable names are ASCII")
            .to_string(),
        kind: RefKind::LegacyRequired,
    })
}

fn is_var_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_var_continue(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    use super::{core_env_vars, envoy_env_vars, path_separator, EnvironmentManager, TraceEvent};
    use crate::error::EnvoyError;

    struct EnvVarGuard {
        previous: Vec<(String, Option<OsString>)>,
    }

    impl EnvVarGuard {
        fn set_many(updates: &[(&str, Option<&OsStr>)]) -> Self {
            let mut previous = Vec::new();

            for (key, value) in updates {
                previous.push(((*key).to_string(), std::env::var_os(key)));
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }

            Self { previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (key, previous) in &self.previous {
                match previous {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// Locks the crate-wide `crate::env_test_lock::MUTEX` rather than a
    /// module-local mutex: several modules' tests mutate the same real
    /// process environment variables, so a single shared lock is required to
    /// prevent cross-module test races under `cargo test`'s default parallel
    /// execution.
    fn with_locked_env<T>(updates: &[(&str, Option<&OsStr>)], test_fn: impl FnOnce() -> T) -> T {
        let _lock = crate::env_test_lock::MUTEX
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env_guard = EnvVarGuard::set_many(updates);
        test_fn()
    }

    fn write_env_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("test env file should be written");
        path
    }

    #[test]
    fn constants_match_python_seed_sets() {
        assert!(core_env_vars().contains("TEMP"));
        assert!(core_env_vars().contains("HOME"));
        assert!(!core_env_vars().contains("PATH"));
        assert!(envoy_env_vars().contains("ENVOY_ALLOWLIST"));
        assert!(envoy_env_vars().contains("ENVOY_BNDL_ROOTS"));
        assert!(envoy_env_vars().contains("ENVOY_CONFIG_ROOT"));
        assert!(envoy_env_vars().contains("DESPATCH_CONFIG_ROOT"));
    }

    #[test]
    fn expand_env_value_supports_required_optional_and_legacy_refs() {
        let env_map = HashMap::from([
            (String::from("ROOT"), String::from("R:/root")),
            (String::from("OPTIONAL"), String::from("opt")),
        ]);
        let special_vars =
            HashMap::from([(String::from("__FILE__"), String::from("/bundle/env.json"))]);

        let expanded = EnvironmentManager::expand_env_value(
            "${ROOT}/${OPTIONAL}/${MISSING}/${?OPTIONAL}/${?ABSENT}/{$ROOT}/${__FILE__}",
            &env_map,
            Some(&special_vars),
        );

        assert_eq!(expanded, "R:/root/opt//opt//R:/root//bundle/env.json");
    }

    #[test]
    fn normalize_path_matches_current_platform() {
        let normalized = EnvironmentManager::normalize_path("R:/one/two");

        if cfg!(windows) {
            assert_eq!(normalized, r"R:\one\two");
        } else {
            assert_eq!(normalized, "R:/one/two");
        }
    }

    #[test]
    fn get_special_variables_uses_bundle_layout() {
        let temp = tempdir().expect("failed to create temp dir");
        let bundle_root = temp.path().join("test_bundle");
        let bundle_env = bundle_root.join(".envoy");
        fs::create_dir_all(&bundle_env).expect("bundle env dir should be created");
        let env_file = write_env_file(&bundle_env, "env.json", "{}");

        let special_vars = EnvironmentManager::get_special_variables(&env_file);
        let expected_bundle = bundle_root.to_string_lossy().replace('\\', "/");
        let expected_bundle_env = bundle_env.to_string_lossy().replace('\\', "/");
        let expected_file = env_file.to_string_lossy().replace('\\', "/");

        assert_eq!(
            special_vars.get("__BUNDLE_NAME__").map(String::as_str),
            Some("test_bundle")
        );
        assert_eq!(
            special_vars.get("__BUNDLE__").map(String::as_str),
            Some(expected_bundle.as_str())
        );
        assert_eq!(
            special_vars.get("__BUNDLE_ENV__").map(String::as_str),
            Some(expected_bundle_env.as_str())
        );
        assert_eq!(
            special_vars.get("__FILE__").map(String::as_str),
            Some(expected_file.as_str())
        );
    }

    #[test]
    fn load_env_from_files_preserves_object_order_for_operators() {
        let temp = tempdir().expect("failed to create temp dir");
        let env_file = write_env_file(
            temp.path(),
            "ordered.json",
            r#"{"MY_PATH":"base","+=MY_PATH":"tail"}"#,
        );
        let manager = EnvironmentManager::new(false, None);

        let result = manager
            .load_env_from_files(&[env_file], None, None, None, None)
            .expect("env file should load");
        let expected = format!("base{}tail", path_separator());

        assert_eq!(
            result.get("MY_PATH").map(String::as_str),
            Some(expected.as_str())
        );
    }

    #[test]
    fn load_env_from_files_applies_all_operator_modes() {
        let temp = tempdir().expect("failed to create temp dir");
        let env_file = write_env_file(
            temp.path(),
            "operators.json",
            r#"{
  "MY_VAR":"base",
  "+=MY_VAR":"tail",
  "^=MY_VAR":"head",
  "?=MY_VAR":"ignored",
  "?=OTHER_VAR":"fallback"
}"#,
        );
        let manager = EnvironmentManager::new(false, None);

        let result = manager
            .load_env_from_files(&[env_file], None, None, None, None)
            .expect("env file should load");
        let expected = format!("head{}base{}tail", path_separator(), path_separator());

        assert_eq!(
            result.get("MY_VAR").map(String::as_str),
            Some(expected.as_str())
        );
        assert_eq!(
            result.get("OTHER_VAR").map(String::as_str),
            Some("fallback")
        );
    }

    #[test]
    fn load_env_from_files_accepts_comment_annotated_json() {
        let temp = tempdir().expect("failed to create temp dir");
        let env_file = write_env_file(
            temp.path(),
            "commented.json",
            r#"{
  // base value
  "MY_VAR":"base",
  "+=MY_VAR":"tail", /* appended segment */
  # default when missing
  "?=OTHER_VAR":"fallback"
}"#,
        );
        let manager = EnvironmentManager::new(false, None);
        let expected_my_var = format!("base{}tail", path_separator());

        let result = manager
            .load_env_from_files(&[env_file], None, None, None, None)
            .expect("env file should load");

        assert_eq!(
            result.get("MY_VAR").map(String::as_str),
            Some(expected_my_var.as_str())
        );
        assert_eq!(
            result.get("OTHER_VAR").map(String::as_str),
            Some("fallback")
        );
    }

    #[test]
    fn load_env_from_files_filters_null_and_unresolved_list_items() {
        let temp = tempdir().expect("failed to create temp dir");
        let env_file = write_env_file(
            temp.path(),
            "lists.json",
            r#"{
  "^=PYTHONPATH":[null,"${MISSING}/pkg","good/path","another/path"],
  "MY_SITE":"${?OPTIONAL_ROOT}/site",
  "ALWAYS":"value"
}"#,
        );
        let manager = EnvironmentManager::new(false, None);

        let result = manager
            .load_env_from_files(&[env_file], None, None, None, None)
            .expect("env file should load");
        let expected = format!(
            "{}{}{}",
            EnvironmentManager::normalize_path("good/path"),
            path_separator(),
            EnvironmentManager::normalize_path("another/path")
        );

        assert_eq!(
            result.get("PYTHONPATH").map(String::as_str),
            Some(expected.as_str())
        );
        assert!(!result.contains_key("MY_SITE"));
        assert_eq!(result.get("ALWAYS").map(String::as_str), Some("value"));
    }

    #[test]
    fn load_env_from_files_uses_cross_file_allowlist_prepass() {
        with_locked_env(
            &[("_ENVOY_TEST_ALLW_CROSS", Some(OsStr::new("host/bin")))],
            || {
                let temp = tempdir().expect("failed to create temp dir");
                let file_a = write_env_file(
                    temp.path(),
                    "file_a.json",
                    r#"{"environment":{"^=_ENVOY_TEST_ALLW_CROSS":"bundle/bin"}}"#,
                );
                let file_b = write_env_file(
                    temp.path(),
                    "file_b.json",
                    r#"{"environment":{},"environment_allowlist":["_ENVOY_TEST_ALLW_CROSS"]}"#,
                );
                let manager = EnvironmentManager::new(false, None);
                let result = manager
                    .load_env_from_files(&[file_a, file_b], None, None, None, None)
                    .expect("env files should load");
                let expected = format!(
                    "{}{}host/bin",
                    EnvironmentManager::normalize_path("bundle/bin"),
                    path_separator()
                );

                assert_eq!(
                    result.get("_ENVOY_TEST_ALLW_CROSS").map(String::as_str),
                    Some(expected.as_str())
                );
            },
        );
    }

    #[test]
    fn load_env_from_files_traces_allowlist_and_steps() {
        with_locked_env(&[("TRACE_ME", Some(OsStr::new("host")))], || {
            let temp = tempdir().expect("failed to create temp dir");
            let env_file = write_env_file(
                temp.path(),
                "trace.json",
                r#"{
  "environment":{
    "^=TRACE_ME":"bundle",
    "?=TRACE_ME":"ignored"
  },
  "environment_allowlist":["TRACE_ME"]
}"#,
            );
            let manager = EnvironmentManager::new(false, None);
            let mut trace = Vec::new();
            let mut allowlist_out = Vec::new();

            let result = manager
                .load_env_from_files(
                    std::slice::from_ref(&env_file),
                    None,
                    Some("TRACE_ME"),
                    Some(&mut trace),
                    Some(&mut allowlist_out),
                )
                .expect("env file should load");
            let expected = format!("bundle{}host", path_separator());

            assert_eq!(allowlist_out, vec![String::from("TRACE_ME")]);
            assert_eq!(
                result.get("TRACE_ME").map(String::as_str),
                Some(expected.as_str())
            );
            assert_eq!(trace.len(), 3);

            match &trace[0] {
                TraceEvent::Allowlist(event) => {
                    assert_eq!(event.file_path, env_file);
                    assert_eq!(event.var_name, "TRACE_ME");
                    assert!(event.seeded);
                    assert_eq!(event.os_value, "host");
                    assert!(!event.already_set);
                }
                other => panic!("expected allowlist event, got {other:?}"),
            }

            match &trace[1] {
                TraceEvent::Step(event) => {
                    assert_eq!(event.operator, "^=");
                    assert_eq!(event.raw_value, "\"bundle\"");
                    assert_eq!(event.value_before, "host");
                    assert_eq!(event.value_after, expected);
                    assert!(event.was_applied);
                }
                other => panic!("expected step event, got {other:?}"),
            }

            match &trace[2] {
                TraceEvent::Step(event) => {
                    assert_eq!(event.operator, "?=");
                    assert_eq!(event.raw_value, "\"ignored\"");
                    assert_eq!(event.value_before, expected);
                    assert_eq!(event.value_after, expected);
                    assert!(!event.was_applied);
                }
                other => panic!("expected step event, got {other:?}"),
            }
        });
    }

    #[test]
    fn prepare_environment_closed_mode_only_seeds_allowed_sets() {
        with_locked_env(
            &[
                ("ENVOY_BNDL_ROOTS", Some(OsStr::new("R:/bundles"))),
                ("CUSTOM_ALLOWED", Some(OsStr::new("allowed"))),
                ("CUSTOM_BLOCKED", Some(OsStr::new("blocked"))),
            ],
            || {
                let manager = EnvironmentManager::new(
                    false,
                    Some(HashSet::from([String::from("CUSTOM_ALLOWED")])),
                );
                let result = manager
                    .prepare_environment(&[], None, None, None)
                    .expect("environment should prepare");

                assert_eq!(
                    result.get("CUSTOM_ALLOWED").map(String::as_str),
                    Some("allowed")
                );
                assert!(!result.contains_key("CUSTOM_BLOCKED"));
                assert_eq!(
                    result.get("ENVOY_BNDL_ROOTS").map(String::as_str),
                    Some("R:/bundles")
                );
            },
        );
    }

    #[test]
    fn a_nested_envoy_inherits_how_the_launcher_chose_it() {
        with_locked_env(
            &[
                ("ENVOY_BIN_LOCK", Some(OsStr::new("C:/app/bin/envoy.exe"))),
                (
                    "ENVOY_BIN_FLOAT",
                    Some(OsStr::new("C:/home/.envoy/bin/envoy.exe")),
                ),
                ("ENVOY_VERSION", Some(OsStr::new("v0.8.0"))),
                ("ENVOY_USER", Some(OsStr::new("someone"))),
                ("ENVOY_PYTHON_API", Some(OsStr::new("off"))),
                // Each envoy sets this for the commands it launches.
                ("ENVOY_PYTHON_PATH", Some(OsStr::new("C:/other/py"))),
            ],
            || {
                let result = EnvironmentManager::new(false, None)
                    .prepare_environment(&[], None, None, None)
                    .expect("environment should prepare");

                for name in [
                    "ENVOY_BIN_LOCK",
                    "ENVOY_BIN_FLOAT",
                    "ENVOY_VERSION",
                    "ENVOY_USER",
                    "ENVOY_PYTHON_API",
                ] {
                    assert!(result.contains_key(name), "{name} should be inherited");
                }
                assert!(!result.contains_key("ENVOY_PYTHON_PATH"));
            },
        );
    }

    #[test]
    fn prepare_environment_inherit_mode_starts_from_full_env() {
        with_locked_env(&[("CUSTOM_INHERITED", Some(OsStr::new("value")))], || {
            let manager = EnvironmentManager::new(true, None);
            let result = manager
                .prepare_environment(&[], None, None, None)
                .expect("environment should prepare");

            assert_eq!(
                result.get("CUSTOM_INHERITED").map(String::as_str),
                Some("value")
            );
        });
    }

    #[test]
    fn prepare_environment_merges_env_file_allowlist_into_envoy_allowlist() {
        with_locked_env(
            &[("ENVOY_ALLOWLIST", Some(OsStr::new("EXISTING_VAR")))],
            || {
                let temp = tempdir().expect("failed to create temp dir");
                let env_file = write_env_file(
                    temp.path(),
                    "allowlist.json",
                    r#"{
  "environment":{},
  "environment_allowlist":["NEW_VAR","EXISTING_VAR"]
}"#,
                );
                let manager = EnvironmentManager::new(false, None);
                let result = manager
                    .prepare_environment(&[env_file], None, None, None)
                    .expect("environment should prepare");

                assert_eq!(
                    result.get("ENVOY_ALLOWLIST").map(String::as_str),
                    Some("EXISTING_VAR;NEW_VAR")
                );
            },
        );
    }

    #[test]
    fn prepare_environment_applies_special_variables_and_explicit_overrides() {
        let temp = tempdir().expect("failed to create temp dir");
        let bundle_root = temp.path().join("bundle");
        let bundle_env = bundle_root.join(".envoy");
        fs::create_dir_all(&bundle_env).expect("bundle env dir should be created");
        let env_file = write_env_file(
            &bundle_env,
            "env.json",
            r#"{
  "BIN":"${__BUNDLE__}/bin",
  "NAME":"${__BUNDLE_NAME__}"
}"#,
        );
        let manager = EnvironmentManager::new(false, None);
        let overrides = HashMap::from([(String::from("NAME"), String::from("override"))]);
        let expected_bin = EnvironmentManager::normalize_path(&format!(
            "{}/bin",
            bundle_root.to_string_lossy().replace('\\', "/")
        ));

        let result = manager
            .prepare_environment(&[env_file], Some(&overrides), None, None)
            .expect("environment should prepare");

        assert_eq!(
            result.get("BIN").map(String::as_str),
            Some(expected_bin.as_str())
        );
        assert_eq!(result.get("NAME").map(String::as_str), Some("override"));
    }

    #[test]
    fn load_env_from_files_reports_invalid_json() {
        let temp = tempdir().expect("failed to create temp dir");
        let env_file = write_env_file(temp.path(), "invalid.json", "{not json");
        let manager = EnvironmentManager::new(false, None);

        let error = manager
            .load_env_from_files(&[env_file], None, None, None, None)
            .expect_err("invalid JSON should fail");

        assert!(matches!(error, EnvoyError::EnvironmentBuild(_)));
        assert!(error
            .to_string()
            .contains("Invalid JSON in environment file"));
    }

    #[test]
    fn diagnose_environment_accepts_comment_annotated_json() {
        let temp = tempdir().expect("failed to create temp dir");
        let env_file = write_env_file(
            temp.path(),
            "diagnose.json",
            r#"{
  // traced value
  "MY_VAR":"base",
  "^=MY_VAR":"head", /* prefix segment */
  # fallback entry
  "?=OTHER_VAR":"fallback"
}"#,
        );
        let manager = EnvironmentManager::new(false, None);
        let expected_my_var = format!("head{}base", path_separator());

        let (result, trace) = manager
            .diagnose_environment(&[env_file], None)
            .expect("diagnostic trace should load");

        assert_eq!(
            result.get("MY_VAR").map(String::as_str),
            Some(expected_my_var.as_str())
        );
        assert_eq!(
            result.get("OTHER_VAR").map(String::as_str),
            Some("fallback")
        );
        assert_eq!(trace.len(), 3);
    }

    #[test]
    fn load_env_from_files_rejects_malformed_pair_entries() {
        let temp = tempdir().expect("failed to create temp dir");
        let env_file = write_env_file(
            temp.path(),
            "malformed.json",
            r#"[["GOOD","value"],["BAD"]]"#,
        );
        let manager = EnvironmentManager::new(false, None);

        let error = manager
            .load_env_from_files(&[env_file], None, None, None, None)
            .expect_err("malformed pair should fail");

        assert!(matches!(error, EnvoyError::EnvironmentBuild(_)));
        assert!(error.to_string().contains("must be a [key, value] pair"));
    }
}
