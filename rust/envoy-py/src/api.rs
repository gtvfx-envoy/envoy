#![allow(clippy::useless_conversion)]

//! Top-level `envoy` API bindings ported from `py/envoy/_api.py`.
//!
//! This module exposes the small convenience surface that historically lived
//! at `envoy.getEnvironment`, `envoy.getAllowlist`, `envoy.traceEnvironment`,
//! `envoy.setApiVerbosity`, `envoy.loadUserConfig`, and
//! `envoy.getCurrentStack`.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::exceptions::envoy_error_to_pyerr;
use envoy_core::bundle_cache::{BundleCache as CoreBundleCache, BundleCacheError};
use envoy_core::bundle_store::load_registry_for_run;
use envoy_core::discovery::{
    discover_bundles_auto as core_discover_bundles_auto, get_bundles as core_get_bundles,
    load_bundles_from_stack as core_load_bundles_from_stack, Bundle as CoreBundle,
    BundleInfo as CoreBundleInfo, BUNDLE_CHECKOUT, BUNDLE_DEFAULT_NAMESPACE,
};
use envoy_core::environment::{
    core_env_vars, envoy_env_vars, EnvironmentManager,
    TraceAllowlistEvent as CoreTraceAllowlistEvent, TraceEvent as CoreTraceEvent,
    TraceStepEvent as CoreTraceStepEvent,
};
use envoy_core::error::EnvoyError;
use envoy_core::runtime::{
    collect_env_files, is_raw_path, load_registry, prepare_env, resolve_team_config_for_bundles,
};
use envoy_core::semver::{
    Constraint as CoreConstraint, SemVer as CoreSemVer, VersionSpec as CoreVersionSpec,
};
use envoy_core::stack::{
    Stack as CoreStack, StackSource as CoreStackSource, DEFAULT_STACK_MAX_DEPTH,
    DEFAULT_STACK_NAMESPACE,
};
use envoy_core::team_config::{TeamConfig as CoreTeamConfig, UserHostConfig as CoreUserHostConfig};
use envoy_core::user_config::UserConfig as CoreUserConfig;
use envoy_core::vcs::{
    detect as core_detect_vcs, VcsAdapter as CoreVcsAdapter, VcsChange as CoreVcsChange,
    VcsKind as CoreVcsKind, VcsStatus as CoreVcsStatus,
};
use pyo3::exceptions::{PyOSError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict, PyModule, PyTuple, PyType};

const SUPPORTED_OPERATING_SYSTEMS: &[&str] = &["Windows", "Linux", "Darwin"];

#[pyclass(module = "envoy")]
struct TraceAllowlistEvent {
    inner: CoreTraceAllowlistEvent,
}

fn vcs_kind_name(kind: CoreVcsKind) -> String {
    kind.as_str().to_string()
}

#[pymethods]
impl TraceAllowlistEvent {
    #[getter]
    fn file_path(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, &self.inner.file_path)
    }

    #[getter]
    fn var_name(&self) -> String {
        self.inner.var_name.clone()
    }

    #[getter]
    fn seeded(&self) -> bool {
        self.inner.seeded
    }

    #[getter]
    fn os_value(&self) -> String {
        self.inner.os_value.clone()
    }

    #[getter]
    fn already_set(&self) -> bool {
        self.inner.already_set
    }

    fn __repr__(&self) -> String {
        format!(
            "TraceAllowlistEvent(file_path='{}', var_name='{}', seeded={}, os_value='{}', \
already_set={})",
            self.inner.file_path.display(),
            self.inner.var_name,
            self.inner.seeded,
            self.inner.os_value,
            self.inner.already_set
        )
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

impl TraceAllowlistEvent {
    fn from_inner(inner: CoreTraceAllowlistEvent) -> Self {
        Self { inner }
    }
}

#[pyclass(module = "envoy")]
struct TraceStepEvent {
    inner: CoreTraceStepEvent,
}

#[pymethods]
impl TraceStepEvent {
    #[getter]
    fn file_path(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, &self.inner.file_path)
    }

    #[getter]
    fn var_name(&self) -> String {
        self.inner.var_name.clone()
    }

    #[getter]
    fn operator(&self) -> String {
        self.inner.operator.clone()
    }

    #[getter]
    fn raw_value(&self) -> String {
        self.inner.raw_value.clone()
    }

    #[getter]
    fn expanded_value(&self) -> String {
        self.inner.expanded_value.clone()
    }

    #[getter]
    fn value_before(&self) -> String {
        self.inner.value_before.clone()
    }

    #[getter]
    fn value_after(&self) -> String {
        self.inner.value_after.clone()
    }

    #[getter]
    fn was_applied(&self) -> bool {
        self.inner.was_applied
    }

    fn __repr__(&self) -> String {
        format!(
            "TraceStepEvent(file_path='{}', var_name='{}', operator='{}', raw_value='{}', \
expanded_value='{}', value_before='{}', value_after='{}', was_applied={})",
            self.inner.file_path.display(),
            self.inner.var_name,
            self.inner.operator,
            self.inner.raw_value,
            self.inner.expanded_value,
            self.inner.value_before,
            self.inner.value_after,
            self.inner.was_applied
        )
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

impl TraceStepEvent {
    fn from_inner(inner: CoreTraceStepEvent) -> Self {
        Self { inner }
    }
}

#[pyclass(module = "envoy")]
struct UserConfig {
    inner: RefCell<CoreUserConfig>,
}

#[pymethods]
impl UserConfig {
    #[classmethod]
    #[pyo3(signature = (path=None))]
    fn load(_cls: &Bound<'_, PyType>, path: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        Ok(Self {
            inner: RefCell::new(CoreUserConfig::load(resolve_optional_path(path)?)),
        })
    }

    #[getter]
    fn path(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, &self.inner.borrow().path)
    }

    fn save(&self) -> PyResult<()> {
        self.inner
            .borrow()
            .save()
            .map_err(user_config_save_to_pyerr)
    }

    fn get(&self, key: &str) -> Option<String> {
        self.inner.borrow().get(key).map(ToOwned::to_owned)
    }

    fn set(&self, key: &str, value: &str) -> PyResult<()> {
        self.inner
            .borrow_mut()
            .set(key, value)
            .map_err(envoy_error_to_pyerr)
    }

    fn unset(&self, key: &str) -> bool {
        self.inner.borrow_mut().unset(key)
    }

    fn items(&self) -> HashMap<String, String> {
        self.inner.borrow().items()
    }

    fn __bool__(&self) -> bool {
        !self.inner.borrow().is_empty()
    }

    fn __repr__(&self) -> String {
        self.inner.borrow().to_string()
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

impl UserConfig {
    fn from_inner(inner: CoreUserConfig) -> Self {
        Self {
            inner: RefCell::new(inner),
        }
    }
}

#[pyclass(module = "envoy._envoy", name = "Bundle")]
struct Bundle {
    inner: CoreBundle,
}

#[pymethods]
impl Bundle {
    #[new]
    #[pyo3(signature = (spec, namespace=None))]
    fn new(spec: &Bound<'_, PyAny>, namespace: Option<&str>) -> PyResult<Self> {
        Ok(Self {
            inner: CoreBundle::new(path_like_to_pathbuf(spec)?, namespace)
                .map_err(envoy_error_to_pyerr)?,
        })
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name().to_string()
    }

    #[getter]
    fn namespace(&self) -> String {
        self.inner.namespace().to_string()
    }

    #[getter]
    fn bndlid(&self) -> String {
        self.inner.bndlid()
    }

    #[getter]
    fn version(&self) -> String {
        self.inner.version()
    }

    #[getter]
    fn is_production(&self) -> bool {
        self.inner.is_production()
    }

    #[getter]
    fn is_checkout(&self) -> bool {
        self.inner.is_checkout()
    }

    #[getter]
    fn path(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, self.inner.path())
    }

    #[getter]
    fn envoy_env(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, self.inner.envoy_env())
    }

    #[getter]
    fn env_files(&self, py: Python<'_>) -> PyResult<PyObject> {
        let dict = PyDict::new_bound(py);
        for (key, value) in self.inner.env_files() {
            dict.set_item(key, path_to_py_path(py, &value)?)?;
        }
        Ok(dict.into_any().unbind())
    }

    #[getter]
    fn commands(&self) -> Vec<String> {
        self.inner.commands()
    }

    fn __repr__(&self) -> String {
        format!(
            "Bundle(bndlid='{}', path={})",
            self.inner.bndlid(),
            self.inner.path().display()
        )
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

impl Bundle {
    fn from_inner(inner: CoreBundle) -> Self {
        Self { inner }
    }
}

#[pyclass(module = "envoy")]
struct BundleInfo {
    inner: CoreBundleInfo,
}

#[pymethods]
impl BundleInfo {
    #[getter]
    fn root(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, &self.inner.root)
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    #[getter]
    fn namespace(&self) -> String {
        self.inner.namespace.clone()
    }

    #[getter]
    fn bndlid(&self) -> String {
        self.inner.bndlid()
    }

    #[getter]
    fn envoy_env(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, self.inner.envoy_env())
    }

    #[getter]
    fn env_files(&self, py: Python<'_>) -> PyResult<PyObject> {
        let dict = PyDict::new_bound(py);
        for (key, value) in self.inner.env_files() {
            dict.set_item(key, path_to_py_path(py, value)?)?;
        }
        Ok(dict.into_any().unbind())
    }

    fn __repr__(&self) -> String {
        format!(
            "BundleInfo(bndlid='{}', root={})",
            self.inner.bndlid(),
            self.inner.root.display()
        )
    }

    fn __str__(&self) -> String {
        format!("{} ({})", self.inner.name, self.inner.root.display())
    }
}

impl BundleInfo {
    fn from_inner(inner: CoreBundleInfo) -> Self {
        Self { inner }
    }
}

#[pyclass(module = "envoy")]
struct Stack {
    inner: CoreStack,
}

#[pymethods]
impl Stack {
    /// Load a strict YAML stack from an `.estack` path.
    #[new]
    fn new(path: &Bound<'_, PyAny>) -> PyResult<Self> {
        CoreStack::new(path_like_to_pathbuf(path)?)
            .map(Self::from_inner)
            .map_err(stack_to_pyerr)
    }

    /// Load the latest publication of a named stack.
    #[classmethod]
    fn from_name(_cls: &Bound<'_, PyType>, name: &str) -> PyResult<Self> {
        CoreStack::from_name(name)
            .map(Self::from_inner)
            .map_err(stack_to_pyerr)
    }

    /// Return the configured or context-resolved current stack.
    #[classmethod]
    #[pyo3(signature = (*, ignore_user_config=false, context=None, default_namespace=DEFAULT_STACK_NAMESPACE, max_depth=DEFAULT_STACK_MAX_DEPTH))]
    fn current(
        _cls: &Bound<'_, PyType>,
        ignore_user_config: bool,
        context: Option<&str>,
        default_namespace: &str,
        max_depth: usize,
    ) -> PyResult<Option<Self>> {
        CoreStack::current(ignore_user_config, context, default_namespace, max_depth)
            .map(|value| value.map(Self::from_inner))
            .map_err(stack_to_pyerr)
    }

    /// Resolve a stack from the named-stack registry for `context`.
    #[classmethod]
    #[pyo3(signature = (context, *, default_namespace=DEFAULT_STACK_NAMESPACE, max_depth=DEFAULT_STACK_MAX_DEPTH))]
    fn resolve(
        _cls: &Bound<'_, PyType>,
        context: &str,
        default_namespace: &str,
        max_depth: usize,
    ) -> PyResult<Self> {
        CoreStack::resolve(context, default_namespace, max_depth)
            .map(Self::from_inner)
            .map_err(stack_to_pyerr)
    }

    #[getter]
    fn path(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, self.inner.path())
    }

    #[getter]
    fn name(&self) -> String {
        self.inner.name().to_string()
    }

    #[getter]
    fn namespace(&self) -> String {
        self.inner.namespace().to_string()
    }

    #[getter]
    fn source(&self, py: Python<'_>) -> PyResult<PyObject> {
        let source = PyDict::new_bound(py);
        match self.inner.source() {
            CoreStackSource::Local { path } => {
                source.set_item("type", "local")?;
                source.set_item("path", path_to_py_path(py, path)?)?;
            }
        }
        Ok(source.into_any().unbind())
    }

    #[getter]
    fn pinned_version(&self) -> Option<String> {
        self.inner.pinned_version().map(ToOwned::to_owned)
    }

    #[getter]
    fn registry_version(&self) -> Option<String> {
        self.inner.registry_version().map(ToOwned::to_owned)
    }

    #[getter]
    fn metadata(&self, py: Python<'_>) -> PyResult<PyObject> {
        json_map_to_pyobject(py, self.inner.metadata())
    }

    #[getter]
    fn bundles(&self, py: Python<'_>) -> PyResult<Vec<Py<Bundle>>> {
        self.inner
            .bundles()
            .map_err(stack_to_pyerr)?
            .into_iter()
            .map(|bundle| Py::new(py, Bundle::from_inner(bundle)))
            .collect()
    }

    #[getter]
    fn commands(&self) -> PyResult<Vec<String>> {
        self.inner.commands().map_err(stack_to_pyerr)
    }

    fn __repr__(&self) -> String {
        self.inner.to_string()
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

impl Stack {
    fn from_inner(inner: CoreStack) -> Self {
        Self { inner }
    }
}

/// Build and return the subprocess environment dict for `command`.
///
/// The environment is for running the command, so the published bundles it
/// needs are copied into the local bundle cache on first use and the
/// environment points at the copies, as `envoy COMMAND` does. Set
/// `ENVOY_CACHE=off` to read bundles where they are instead.
#[pyfunction(name = "getEnvironment")]
#[pyo3(signature = (command, *, inherit_env=false, allowlist=None, bundle_roots=None, commands_file=None))]
fn get_environment(
    command: &str,
    inherit_env: bool,
    allowlist: Option<Vec<String>>,
    bundle_roots: Option<Vec<String>>,
    commands_file: Option<&Bound<'_, PyAny>>,
) -> PyResult<HashMap<String, String>> {
    if is_raw_path(command) {
        let allowlist_set = allowlist_to_hashset(allowlist);
        return EnvironmentManager::new(inherit_env, allowlist_set)
            .prepare_environment(&[], None, None, None)
            .map_err(envoy_error_to_pyerr);
    }

    let commands_file = resolve_optional_path(commands_file)?;
    // The environment is for running, so the bundles it needs come from the
    // local store, as they do for `envoy COMMAND`.
    let (registry, bundles) = load_registry_for_run(
        bundle_roots.as_deref(),
        commands_file.as_deref(),
        command,
        None,
    )
    .map_err(envoy_error_to_pyerr)?;
    let (env, _) = prepare_env(
        command,
        &registry,
        bundles.as_deref(),
        inherit_env,
        allowlist.as_deref(),
        None,
    )
    .map_err(envoy_error_to_pyerr)?;

    Ok(env)
}

/// Return the default set of system variable names envoy seeds in closed mode.
#[pyfunction(name = "getAllowlist", signature = (extra=None))]
fn get_allowlist(py: Python<'_>, extra: Option<Vec<String>>) -> PyResult<PyObject> {
    let values = build_allowlist(extra);
    let builtins = PyModule::import_bound(py, "builtins")?;
    let frozenset = builtins.getattr("frozenset")?;

    Ok(frozenset.call1((values,))?.into_any().unbind())
}

/// Build the environment for `command` and return a trace of how `var` mutated.
///
/// Tracing only looks, so bundles are read where they are and nothing is
/// copied into the bundle cache.
#[pyfunction(name = "traceEnvironment")]
#[pyo3(signature = (command, var, *, inherit_env=false, allowlist=None, bundle_roots=None, commands_file=None))]
fn trace_environment(
    py: Python<'_>,
    command: &str,
    var: &str,
    inherit_env: bool,
    allowlist: Option<Vec<String>>,
    bundle_roots: Option<Vec<String>>,
    commands_file: Option<&Bound<'_, PyAny>>,
) -> PyResult<(HashMap<String, String>, Vec<PyObject>)> {
    let allowlist_set = allowlist_to_hashset(allowlist.clone());
    let env_manager = EnvironmentManager::new(inherit_env, allowlist_set);
    let mut trace_events = Vec::new();

    let final_env = if is_raw_path(command) {
        env_manager
            .prepare_environment(&[], None, Some(var), Some(&mut trace_events))
            .map_err(envoy_error_to_pyerr)?
    } else {
        let commands_file = resolve_optional_path(commands_file)?;
        // Tracing only looks, so bundles are read where they are.
        let (registry, bundles) =
            load_registry(bundle_roots.as_deref(), commands_file.as_deref(), None)
                .map_err(envoy_error_to_pyerr)?;
        let env_files = collect_env_files(command, &registry, bundles.as_deref())
            .map_err(envoy_error_to_pyerr)?;
        env_manager
            .prepare_environment(&env_files, None, Some(var), Some(&mut trace_events))
            .map_err(envoy_error_to_pyerr)?
    };

    let trace_out = trace_events
        .into_iter()
        .map(|event| trace_event_to_pyobject(py, event))
        .collect::<PyResult<Vec<_>>>()?;

    Ok((final_env, trace_out))
}

/// Build the environment for `command` and return a full diagnostic trace of
/// how every variable was resolved across all env files.
///
/// Unlike [`trace_environment`](trace_environment), which traces a single
/// variable, this walks **all** entries in all env files and returns one
/// trace event per entry plus allowlist pre-pass events. Suitable for
/// diagnostic / debugging output. Like tracing, it reads bundles where they
/// are and copies nothing into the bundle cache.
#[pyfunction(name = "diagnoseEnvironment")]
#[pyo3(signature = (command, *, inherit_env=false, allowlist=None, bundle_roots=None, commands_file=None))]
fn diagnose_environment(
    py: Python<'_>,
    command: &str,
    inherit_env: bool,
    allowlist: Option<Vec<String>>,
    bundle_roots: Option<Vec<String>>,
    commands_file: Option<&Bound<'_, PyAny>>,
) -> PyResult<(HashMap<String, String>, Vec<PyObject>)> {
    let allowlist_set = allowlist_to_hashset(allowlist.clone());
    let env_manager = EnvironmentManager::new(inherit_env, allowlist_set);

    let (final_env, trace_events) = if is_raw_path(command) {
        env_manager
            .diagnose_environment(&[], None)
            .map_err(envoy_error_to_pyerr)?
    } else {
        let commands_file = resolve_optional_path(commands_file)?;
        // Diagnosing only looks, so bundles are read where they are.
        let (registry, bundles) =
            load_registry(bundle_roots.as_deref(), commands_file.as_deref(), None)
                .map_err(envoy_error_to_pyerr)?;
        let env_files = collect_env_files(command, &registry, bundles.as_deref())
            .map_err(envoy_error_to_pyerr)?;
        env_manager
            .diagnose_environment(&env_files, None)
            .map_err(envoy_error_to_pyerr)?
    };

    let trace_out = trace_events
        .into_iter()
        .map(|event| trace_event_to_pyobject(py, event))
        .collect::<PyResult<Vec<_>>>()?;

    Ok((final_env, trace_out))
}

/// Set the logging verbosity for the `envoy` logger.
#[pyfunction(name = "setApiVerbosity")]
fn set_api_verbosity(level: &Bound<'_, PyAny>) -> PyResult<()> {
    let py = level.py();
    let logging = PyModule::import_bound(py, "logging")?;
    let logger = logging.getattr("getLogger")?.call1(("envoy",))?;
    logger.call_method1("setLevel", (level,))?;
    Ok(())
}

/// Load the persistent user config from disk.
#[pyfunction(name = "loadUserConfig", signature = (path=None))]
fn load_user_config(py: Python<'_>, path: Option<&Bound<'_, PyAny>>) -> PyResult<Py<UserConfig>> {
    Py::new(
        py,
        UserConfig::from_inner(CoreUserConfig::load(resolve_optional_path(path)?)),
    )
}

/// Return the configured or context-resolved current stack.
#[pyfunction(name = "getCurrentStack")]
#[pyo3(signature = (*, ignore_user_config=false, context=None, default_namespace=DEFAULT_STACK_NAMESPACE, max_depth=DEFAULT_STACK_MAX_DEPTH))]
fn get_current_stack(
    py: Python<'_>,
    ignore_user_config: bool,
    context: Option<&str>,
    default_namespace: &str,
    max_depth: usize,
) -> PyResult<Option<Py<Stack>>> {
    let stack = CoreStack::current(ignore_user_config, context, default_namespace, max_depth)
        .map_err(stack_to_pyerr)?
        .map(Stack::from_inner);

    stack.map(|value| Py::new(py, value)).transpose()
}

/// Return the active team configuration resolved from discovered bundles.
///
/// Returns `None` when no discovered bundle defines a `.envoy/team.json`.
/// This is the automatic-discovery counterpart to constructing a
/// [`TeamConfig`] by hand via `TeamConfig.load_from_file`.
#[pyfunction(name = "getCurrentTeamConfig")]
#[pyo3(signature = (*, bundle_roots=None, commands_file=None))]
fn get_current_team_config(
    py: Python<'_>,
    bundle_roots: Option<Vec<String>>,
    commands_file: Option<&Bound<'_, PyAny>>,
) -> PyResult<Option<Py<TeamConfig>>> {
    let commands_file = resolve_optional_path(commands_file)?;
    let (_registry, bundles) =
        load_registry(bundle_roots.as_deref(), commands_file.as_deref(), None)
            .map_err(envoy_error_to_pyerr)?;

    resolve_team_config_for_bundles(bundles.as_deref())
        .map(|inner| Py::new(py, TeamConfig { inner }))
        .transpose()
}

#[pyfunction(name = "discoverBundlesAuto")]
fn discover_bundles_auto(py: Python<'_>) -> PyResult<Vec<Py<BundleInfo>>> {
    bundle_infos_to_py(
        py,
        core_discover_bundles_auto().map_err(envoy_error_to_pyerr)?,
    )
}

#[pyfunction(name = "getBundles", signature = (stack_file=None))]
fn get_bundles(
    py: Python<'_>,
    stack_file: Option<&Bound<'_, PyAny>>,
) -> PyResult<Vec<Py<BundleInfo>>> {
    let stack_file = resolve_optional_path(stack_file)?;
    bundle_infos_to_py(
        py,
        core_get_bundles(stack_file.as_deref()).map_err(envoy_error_to_pyerr)?,
    )
}

#[pyfunction(name = "loadBundlesFromStack")]
fn load_bundles_from_stack(
    py: Python<'_>,
    stack_file: &Bound<'_, PyAny>,
) -> PyResult<Vec<Py<BundleInfo>>> {
    bundle_infos_to_py(
        py,
        core_load_bundles_from_stack(&path_like_to_pathbuf(stack_file)?)
            .map_err(envoy_error_to_pyerr)?,
    )
}

pub fn register_api_bindings(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("OPERATING_SYSTEM", current_operating_system())?;
    m.add(
        "SUPPORTED_OPERATING_SYSTEMS",
        PyTuple::new_bound(py, SUPPORTED_OPERATING_SYSTEMS),
    )?;
    m.add("BUNDLE_CHECKOUT", BUNDLE_CHECKOUT)?;
    m.add("BUNDLE_DEFAULT_NAMESPACE", BUNDLE_DEFAULT_NAMESPACE)?;
    m.add_class::<TraceAllowlistEvent>()?;
    m.add_class::<TraceStepEvent>()?;
    m.add_class::<UserConfig>()?;
    m.add_class::<Bundle>()?;
    m.add_class::<BundleInfo>()?;
    m.add_class::<Stack>()?;
    m.add_class::<BundleCache>()?;
    m.add_class::<TeamConfig>()?;
    m.add_class::<UserHostConfig>()?;
    m.add_class::<Vcs>()?;
    m.add_class::<VcsChange>()?;
    m.add_class::<VcsStatus>()?;
    m.add_class::<SemVer>()?;
    m.add_class::<Constraint>()?;
    m.add_class::<VersionSpec>()?;
    m.add_function(wrap_pyfunction!(get_environment, m)?)?;
    m.add_function(wrap_pyfunction!(get_allowlist, m)?)?;
    m.add_function(wrap_pyfunction!(trace_environment, m)?)?;
    m.add_function(wrap_pyfunction!(diagnose_environment, m)?)?;
    m.add_function(wrap_pyfunction!(set_api_verbosity, m)?)?;
    m.add_function(wrap_pyfunction!(load_user_config, m)?)?;
    m.add_function(wrap_pyfunction!(get_current_stack, m)?)?;
    m.add_function(wrap_pyfunction!(get_current_team_config, m)?)?;
    m.add_function(wrap_pyfunction!(discover_bundles_auto, m)?)?;
    m.add_function(wrap_pyfunction!(get_bundles, m)?)?;
    m.add_function(wrap_pyfunction!(load_bundles_from_stack, m)?)?;
    Ok(())
}

fn current_operating_system() -> &'static str {
    map_operating_system_name(std::env::consts::OS)
}

fn map_operating_system_name(name: &str) -> &str {
    match name {
        "windows" => "Windows",
        "linux" => "Linux",
        "macos" => "Darwin",
        other => other,
    }
}

fn build_allowlist(extra: Option<Vec<String>>) -> Vec<String> {
    let mut values = core_env_vars()
        .iter()
        .chain(envoy_env_vars().iter())
        .map(|value| (*value).to_string())
        .collect::<BTreeSet<_>>();

    if let Some(extra) = extra {
        values.extend(extra);
    }

    values.into_iter().collect()
}

fn allowlist_to_hashset(
    allowlist: Option<Vec<String>>,
) -> Option<std::collections::HashSet<String>> {
    allowlist.map(|values| values.into_iter().collect())
}

fn trace_event_to_pyobject(py: Python<'_>, event: CoreTraceEvent) -> PyResult<PyObject> {
    match event {
        CoreTraceEvent::Allowlist(event) => {
            Ok(Py::new(py, TraceAllowlistEvent::from_inner(event))?
                .into_bound(py)
                .into_any()
                .unbind())
        }
        CoreTraceEvent::Step(event) => Ok(Py::new(py, TraceStepEvent::from_inner(event))?
            .into_bound(py)
            .into_any()
            .unbind()),
    }
}

fn bundle_infos_to_py(py: Python<'_>, infos: Vec<CoreBundleInfo>) -> PyResult<Vec<Py<BundleInfo>>> {
    infos
        .into_iter()
        .map(|info| Py::new(py, BundleInfo::from_inner(info)))
        .collect()
}

fn resolve_optional_path(value: Option<&Bound<'_, PyAny>>) -> PyResult<Option<PathBuf>> {
    value.map(path_like_to_pathbuf).transpose()
}

fn path_like_to_pathbuf(value: &Bound<'_, PyAny>) -> PyResult<PathBuf> {
    let py = value.py();
    let os = PyModule::import_bound(py, "os")?;
    let path_value = os.getattr("fspath")?.call1((value,))?;
    if let Ok(text) = path_value.extract::<String>() {
        return Ok(PathBuf::from(text));
    }
    if let Ok(bytes) = path_value.extract::<Vec<u8>>() {
        return Ok(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()));
    }

    Err(PyTypeError::new_err("Expected a path-like object"))
}

fn path_to_py_path(py: Python<'_>, path: &Path) -> PyResult<PyObject> {
    let pathlib = PyModule::import_bound(py, "pathlib")?;
    Ok(pathlib
        .getattr("Path")?
        .call1((path.to_string_lossy().into_owned(),))?
        .into_any()
        .unbind())
}

fn user_config_save_to_pyerr(error: EnvoyError) -> PyErr {
    match error {
        other @ (EnvoyError::Io { .. } | EnvoyError::Json { .. }) => {
            PyOSError::new_err(other.to_string())
        }
        other => envoy_error_to_pyerr(other),
    }
}

fn stack_to_pyerr(error: EnvoyError) -> PyErr {
    envoy_error_to_pyerr(error)
}

fn json_map_to_pyobject(
    py: Python<'_>,
    values: &HashMap<String, serde_json::Value>,
) -> PyResult<PyObject> {
    let serialized =
        serde_json::to_string(values).map_err(|error| PyValueError::new_err(error.to_string()))?;
    Ok(PyModule::import_bound(py, "json")?
        .getattr("loads")?
        .call1((serialized,))?
        .into_any()
        .unbind())
}

// ---------------------------------------------------------------------------
// SemVer Python bindings
// ---------------------------------------------------------------------------

/// Python wrapper for semantic versions.
#[pyclass(module = "envoy")]
#[derive(Clone)]
struct SemVer {
    inner: CoreSemVer,
}

#[pymethods]
impl SemVer {
    /// Parse a version string with or without a leading `v`.
    #[staticmethod]
    fn parse(value: &str) -> PyResult<Self> {
        let parsed = CoreSemVer::parse(value).map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self { inner: parsed })
    }

    /// Return the prerelease label without the numeric suffix.
    #[getter]
    fn prerelease_label(&self) -> Option<String> {
        self.inner.prerelease_label().map(|s| s.to_string())
    }

    /// Return the numeric prerelease suffix, if present.
    #[getter]
    fn prerelease_number(&self) -> Option<u64> {
        self.inner.prerelease_number()
    }

    /// Return a copy with `major` incremented and lower parts reset.
    fn bump_major(&self) -> Self {
        Self {
            inner: self.inner.bump_major(),
        }
    }

    /// Return a copy with `minor` incremented and lower parts reset.
    fn bump_minor(&self) -> Self {
        Self {
            inner: self.inner.bump_minor(),
        }
    }

    /// Return a copy with `patch` incremented and prerelease cleared.
    fn bump_patch(&self) -> Self {
        Self {
            inner: self.inner.bump_patch(),
        }
    }

    /// Render the version as a git tag string with a leading `v`.
    fn to_tag(&self) -> String {
        self.inner.to_tag()
    }

    /// Return `true` if this is a prerelease version.
    #[getter]
    fn is_prerelease(&self) -> bool {
        self.inner.is_prerelease()
    }

    // Comparison operators
    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    fn __ne__(&self, other: &Self) -> bool {
        self.inner != other.inner
    }

    fn __lt__(&self, other: &Self) -> bool {
        self.inner < other.inner
    }

    fn __le__(&self, other: &Self) -> bool {
        self.inner <= other.inner
    }

    fn __gt__(&self, other: &Self) -> bool {
        self.inner > other.inner
    }

    fn __ge__(&self, other: &Self) -> bool {
        self.inner >= other.inner
    }

    // Display
    fn __repr__(&self) -> String {
        format!("SemVer('{}')", self.inner)
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    // Attribute accessors for major, minor, patch
    #[getter]
    fn major(&self) -> u64 {
        self.inner.major
    }

    #[getter]
    fn minor(&self) -> u64 {
        self.inner.minor
    }

    #[getter]
    fn patch(&self) -> u64 {
        self.inner.patch
    }
}

/// Python wrapper for version constraints.
#[pyclass(module = "envoy")]
struct Constraint {
    inner: CoreConstraint,
}

#[pymethods]
impl Constraint {
    /// Parse a single constraint from a string like `>=1.0.0`, `^1.2`, etc.
    #[staticmethod]
    fn parse(input: &str) -> PyResult<Self> {
        let parsed =
            CoreConstraint::parse(input).map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self { inner: parsed })
    }

    /// Test whether `version` satisfies this constraint.
    fn matches(&self, version: &SemVer) -> bool {
        self.inner.matches(&version.inner)
    }

    // Display
    fn __repr__(&self) -> String {
        format!("Constraint('{}')", self.inner)
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }
}

/// Python wrapper for version specs (comma-separated constraints).
#[pyclass(module = "envoy")]
struct VersionSpec {
    inner: CoreVersionSpec,
}

#[pymethods]
impl VersionSpec {
    /// Parse a version spec string like `>=1.0.0,<2.0.0` or `^1.2`.
    #[staticmethod]
    fn parse(input: &str) -> PyResult<Self> {
        let parsed =
            CoreVersionSpec::parse(input).map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self { inner: parsed })
    }

    /// Test whether `version` satisfies all constraints in this spec.
    fn matches(&self, version: &SemVer) -> bool {
        self.inner.matches(&version.inner)
    }

    // Display
    fn __repr__(&self) -> String {
        format!("VersionSpec('{}')", self.inner)
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }
}

/// Python wrapper for the content-addressed bundle cache.
///
/// Bundles are stored under `<cache_root>/<content_hash>/` and indexed by
/// logical bundle ID + version in a JSON manifest at `<cache_root>/.index.json`.
#[pyclass(module = "envoy")]
struct BundleCache {
    inner: CoreBundleCache,
}

#[pymethods]
impl BundleCache {
    /// Open (or create) a bundle cache at `root`.
    #[new]
    fn new(root: &Bound<'_, PyAny>) -> PyResult<Self> {
        let path = path_like_to_pathbuf(root)?;
        let inner = CoreBundleCache::new(&path).map_err(|e| PyOSError::new_err(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Store a bundle directory in the cache and return its metadata.
    fn store(
        &mut self,
        py: Python<'_>,
        bundle_id: &str,
        version: &str,
        source_dir: &Bound<'_, PyAny>,
    ) -> PyResult<PyObject> {
        let path = path_like_to_pathbuf(source_dir)?;
        let cached = self
            .inner
            .store(bundle_id, version, &path)
            .map_err(|e| PyOSError::new_err(e.to_string()))?;

        // Return a dict with content_hash, path, and last_accessed.
        let dict = PyDict::new_bound(py);
        dict.set_item("content_hash", cached.content_hash)?;
        dict.set_item(
            "path",
            path_to_py_path(py, &cached.path).map_err(|e| PyOSError::new_err(e.to_string()))?,
        )?;
        dict.set_item("last_accessed", cached.last_accessed)?;
        Ok(dict.into_any().unbind())
    }

    /// Retrieve a previously stored bundle by ID and version.
    fn get(&self, py: Python<'_>, bundle_id: &str, version: &str) -> PyResult<Option<PyObject>> {
        match self.inner.get(bundle_id, version) {
            Ok(cached) => {
                let dict = PyDict::new_bound(py);
                dict.set_item("content_hash", cached.content_hash)?;
                dict.set_item(
                    "path",
                    path_to_py_path(py, &cached.path)
                        .map_err(|e| PyOSError::new_err(e.to_string()))?,
                )?;
                dict.set_item("last_accessed", cached.last_accessed)?;
                Ok(Some(dict.into_any().unbind()))
            }
            Err(BundleCacheError::NotFound { .. }) => Ok(None),
            Err(e) => Err(PyOSError::new_err(e.to_string())),
        }
    }

    /// List all bundles in the cache.
    fn list(&self) -> Vec<(String, String)> {
        self.inner
            .list()
            .into_iter()
            .map(|(bundle_id, version)| (bundle_id.to_string(), version.to_string()))
            .collect()
    }

    /// Remove a bundle from the cache by ID and version.
    fn remove(&mut self, bundle_id: &str, version: &str) -> PyResult<bool> {
        match self.inner.remove(bundle_id, version) {
            Ok(removed) => Ok(removed),
            Err(e) => Err(PyOSError::new_err(e.to_string())),
        }
    }

    /// Compact the cache by removing unreferenced content and applying retention policies.
    fn compact(&mut self) -> PyResult<usize> {
        let evicted = self
            .inner
            .compact()
            .map_err(|e| PyOSError::new_err(e.to_string()))?;
        Ok(evicted)
    }

    /// Get the cache root path.
    #[getter]
    fn root(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, self.inner.root()).map_err(|e| PyOSError::new_err(e.to_string()))
    }
}

/// Python wrapper for a resolved team configuration.
#[pyclass(module = "envoy")]
struct TeamConfig {
    inner: CoreTeamConfig,
}

#[pymethods]
impl TeamConfig {
    /// Human-readable team name (e.g., `"bfd"`).
    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    /// Absolute path to the production bundles root directory.
    #[getter]
    fn prod_bundles_root(&self) -> Option<String> {
        self.inner
            .prod_bundles_root
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// Absolute path to the production stacks root directory.
    #[getter]
    fn prod_stacks_root(&self) -> Option<String> {
        self.inner
            .prod_stacks_root
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// Path (possibly with `~` expansion) to a user/host config JSON file.
    #[getter]
    fn user_host_config_file(&self) -> Option<String> {
        self.inner.user_host_config_file.clone()
    }

    /// Arbitrary additional settings from team.json.
    #[getter]
    fn metadata(&self) -> HashMap<String, String> {
        self.inner
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::to_string(v).unwrap_or_default()))
            .collect()
    }

    /// Return the team name.
    fn __repr__(&self) -> String {
        format!("TeamConfig('{}')", self.inner.name)
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

/// Python wrapper for a normalized VCS change entry.
#[pyclass(module = "envoy")]
struct VcsChange {
    inner: CoreVcsChange,
}

#[pymethods]
impl VcsChange {
    /// Repository-relative path for the changed item.
    #[getter]
    fn path(&self) -> String {
        self.inner.path.clone()
    }

    /// Normalized change status.
    #[getter]
    fn status(&self) -> String {
        self.inner.status.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "VcsChange(path='{}', status='{}')",
            self.inner.path, self.inner.status
        )
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

/// Python wrapper for a normalized VCS status snapshot.
#[pyclass(module = "envoy")]
struct VcsStatus {
    inner: CoreVcsStatus,
}

#[pymethods]
impl VcsStatus {
    /// Backend kind for this status snapshot.
    #[getter]
    fn kind(&self) -> String {
        vcs_kind_name(self.inner.kind)
    }

    /// Working-copy root used for the status query.
    #[getter]
    fn root(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, &self.inner.root).map_err(|e| PyOSError::new_err(e.to_string()))
    }

    /// Normalized change entries returned by the backend.
    #[getter]
    fn changes(&self, py: Python<'_>) -> PyResult<Vec<Py<VcsChange>>> {
        self.inner
            .changes
            .iter()
            .cloned()
            .map(|inner| Py::new(py, VcsChange { inner }))
            .collect()
    }

    fn __repr__(&self) -> String {
        format!(
            "VcsStatus(kind='{}', root='{}', changes={})",
            vcs_kind_name(self.inner.kind),
            self.inner.root.display(),
            self.inner.changes.len()
        )
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

/// Python wrapper for VCS detection and status queries.
#[pyclass(module = "envoy")]
struct Vcs {
    inner: Box<dyn CoreVcsAdapter>,
}

#[pymethods]
impl Vcs {
    /// Detect the backend for the current working directory.
    #[classmethod]
    fn detect(_cls: &Bound<'_, PyType>) -> PyResult<Option<Self>> {
        let current_dir = std::env::current_dir().map_err(|e| PyOSError::new_err(e.to_string()))?;
        Ok(core_detect_vcs(&current_dir).map(|inner| Self { inner }))
    }

    /// Detected backend kind.
    #[getter]
    fn kind(&self) -> String {
        vcs_kind_name(self.inner.kind())
    }

    /// Working-copy root for the detected backend.
    #[getter]
    fn root(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, self.inner.root()).map_err(|e| PyOSError::new_err(e.to_string()))
    }

    /// Return a normalized status snapshot for the working copy.
    fn status(&self, py: Python<'_>) -> PyResult<Py<VcsStatus>> {
        let inner = self
            .inner
            .status()
            .map_err(|e| PyOSError::new_err(e.to_string()))?;
        Py::new(py, VcsStatus { inner })
    }

    /// Return normalized change entries for the working copy.
    fn get_changes(&self, py: Python<'_>) -> PyResult<Vec<Py<VcsChange>>> {
        self.inner
            .get_changes()
            .map_err(|e| PyOSError::new_err(e.to_string()))?
            .into_iter()
            .map(|inner| Py::new(py, VcsChange { inner }))
            .collect()
    }

    fn __repr__(&self) -> String {
        format!(
            "Vcs(kind='{}', root='{}')",
            vcs_kind_name(self.inner.kind()),
            self.inner.root().display()
        )
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}
/// Python wrapper for per-user/host configuration that overrides team defaults.
#[pyclass(module = "envoy")]
struct UserHostConfig {
    inner: CoreUserHostConfig,
}

#[pymethods]
impl UserHostConfig {
    /// Override for the production bundles root (empty string = use team default).
    #[getter]
    fn prod_bundles_root(&self) -> String {
        self.inner.prod_bundles_root.clone()
    }

    /// Set override for the production bundles root.
    #[setter]
    fn set_prod_bundles_root(&mut self, path: &str) {
        self.inner.prod_bundles_root = path.to_string();
    }

    /// Override for the production stacks root (empty string = use team default).
    #[getter]
    fn prod_stacks_root(&self) -> String {
        self.inner.prod_stacks_root.clone()
    }

    /// Set override for the production stacks root.
    #[setter]
    fn set_prod_stacks_root(&mut self, path: &str) {
        self.inner.prod_stacks_root = path.to_string();
    }

    /// Arbitrary additional settings from user/host config.
    #[getter]
    fn metadata(&self) -> HashMap<String, String> {
        self.inner
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::to_string(v).unwrap_or_default()))
            .collect()
    }

    /// Return the user/host config as a dict.
    fn __repr__(&self) -> String {
        format!(
            "UserHostConfig(prod_bundles_root='{}', prod_stacks_root='{}', metadata={})",
            self.inner.prod_bundles_root,
            self.inner.prod_stacks_root,
            if !self.inner.metadata.is_empty() {
                "..."
            } else {
                ""
            }
        )
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

#[cfg(test)]
mod tests {
    use super::{build_allowlist, map_operating_system_name};

    #[test]
    fn os_name_mapping_matches_python_platform_system_names() {
        assert_eq!(map_operating_system_name("windows"), "Windows");
        assert_eq!(map_operating_system_name("linux"), "Linux");
        assert_eq!(map_operating_system_name("macos"), "Darwin");
    }

    #[test]
    fn unknown_os_names_pass_through() {
        assert_eq!(map_operating_system_name("freebsd"), "freebsd");
    }

    #[test]
    fn allowlist_contains_envoy_roots_and_extra_values() {
        let allowlist = build_allowlist(Some(vec![String::from("EXTRA_VAR")]));

        assert!(allowlist.contains(&String::from("ENVOY_BNDL_ROOTS")));
        assert!(allowlist.contains(&String::from("EXTRA_VAR")));
    }
}
