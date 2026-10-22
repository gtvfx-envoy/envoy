//! Top-level `envoy.Environment` class — dict-like, auto-initializing.
//!
//! This module provides the modernized `Environment` API that replaces the
//! legacy `envoy.proc.Environment`. The new design:
//!
//! - Lives at the top level (`import envoy; env = envoy.Environment("cmd")`)
//! - Implements dict-like access (`env["VAR"]`, `env.get("VAR")`)
//! - Auto-builds on first attribute access (no explicit `.build()` step)
//! - Caches the built environment for reuse

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use envoy_core::bundle_store::load_registry_for_run;
use envoy_core::runtime::{is_raw_path, prepare_env};
use pyo3::exceptions::{PyKeyError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString};

/// Internal cached environment data (built once, reused).
#[derive(Clone)]
struct CachedEnv {
    env: HashMap<String, String>,
}

/// Top-level `envoy.Environment` class.
///
/// Wraps a command name or raw executable path and lazily builds the
/// subprocess environment on first access. Implements dict-like methods for
/// convenient variable lookup.
#[pyclass(module = "envoy")]
pub struct Environment {
    /// Command name or raw executable path.
    command: String,
    /// Whether to inherit parent environment (closed vs open mode).
    inherit_env: bool,
    /// Additional variables to seed in closed mode.
    allowlist: Option<Vec<String>>,
    /// Optional explicit bundle roots.
    bundle_roots: Option<Vec<String>>,
    /// Optional fallback commands.json path.
    commands_file: Option<PathBuf>,
    /// Cached built environment (built on first access).
    cached: Mutex<Option<CachedEnv>>,
}

#[pymethods]
impl Environment {
    /// Create a new `envoy.Environment` for the given command.
    ///
    /// The environment is built lazily on first attribute access — no
    /// explicit `.build()` step required.
    ///
    /// Args:
    ///     command: Envoy command name or raw executable path.
    ///     inherit_env: When ``False`` the child receives a closed
    ///         environment seeded only with envoy's core allowlisted
    ///         variables and the resolved env-file values.
    ///     allowlist: Additional variable names to seed in closed mode.
    ///     bundle_roots: Optional explicit bundle roots replacing
    ///         ``ENVOY_BNDL_ROOTS`` for this instance.
    ///     commands_file: Optional fallback ``commands.json`` path.
    #[new]
    #[pyo3(signature = (command, *, inherit_env=false, allowlist=None, bundle_roots=None, commands_file=None))]
    fn new(
        py: Python<'_>,
        command: String,
        inherit_env: bool,
        allowlist: Option<Vec<String>>,
        bundle_roots: Option<Vec<String>>,
        commands_file: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        Ok(Self {
            command,
            inherit_env,
            allowlist,
            bundle_roots,
            commands_file: commands_file
                .map(|value| path_like_to_pathbuf(py, value))
                .transpose()?,
            cached: Mutex::new(None),
        })
    }

    /// Return the command name or raw path this environment was created for.
    #[getter]
    fn command(&self) -> String {
        self.command.clone()
    }

    /// Return whether this environment inherits from the parent process.
    #[getter]
    fn inherit_env(&self) -> bool {
        self.inherit_env
    }

    /// Return the allowlist passed at construction time.
    #[getter]
    fn allowlist(&self) -> Vec<String> {
        self.allowlist.clone().unwrap_or_default()
    }

    /// Get an environment variable by name (dict-like access).
    ///
    /// Args:
    ///     key: Environment variable name.
    ///
    /// Returns:
    ///     The variable value, or raises ``KeyError`` if not present.
    fn __getitem__(&self, py: Python<'_>, key: &str) -> PyResult<PyObject> {
        let env = self.get_env(py)?;
        match env.get(key) {
            Some(value) => Ok(PyString::new_bound(py, value).into_any().unbind()),
            None => Err(PyKeyError::new_err(format!("'{}'", key))),
        }
    }

    /// Get an environment variable by name with optional default.
    ///
    /// Args:
    ///     key: Environment variable name.
    ///     default: Value to return if the variable is not present.
    ///         Defaults to ``None``.
    ///
    /// Returns:
    ///     The variable value, or ``default`` if not present.
    #[pyo3(signature = (key, default=None))]
    fn get(&self, py: Python<'_>, key: &str, default: Option<PyObject>) -> PyResult<PyObject> {
        let env = self.get_env(py)?;
        match env.get(key) {
            Some(value) => Ok(PyString::new_bound(py, value).into_any().unbind()),
            None => Ok(default.unwrap_or_else(|| py.None())),
        }
    }

    /// Return all environment variable names.
    fn keys(&self, py: Python<'_>) -> PyResult<PyObject> {
        let env = self.get_env(py)?;
        let keys: Vec<&str> = env.keys().map(|k| k.as_str()).collect();
        Ok(PyList::new_bound(py, &keys).into_any().unbind())
    }

    /// Return all environment variable values.
    fn values(&self, py: Python<'_>) -> PyResult<PyObject> {
        let env = self.get_env(py)?;
        let values: Vec<&str> = env.values().map(|v| v.as_str()).collect();
        Ok(PyList::new_bound(py, &values).into_any().unbind())
    }

    /// Return (key, value) pairs.
    fn items(&self, py: Python<'_>) -> PyResult<PyObject> {
        let env = self.get_env(py)?;
        let items: Vec<(String, String)> =
            env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        Ok(PyList::new_bound(py, &items).into_any().unbind())
    }

    /// Check if an environment variable is present.
    fn __contains__(&self, py: Python<'_>, key: &str) -> PyResult<bool> {
        let env = self.get_env(py)?;
        Ok(env.contains_key(key))
    }

    /// Return the number of environment variables.
    fn __len__(&self, py: Python<'_>) -> PyResult<usize> {
        let env = self.get_env(py)?;
        Ok(env.len())
    }

    /// Iterate over environment variable names.
    fn __iter__(&self, py: Python<'_>) -> PyResult<PyObject> {
        let env = self.get_env(py)?;
        let keys: Vec<&str> = env.keys().map(|k| k.as_str()).collect();
        Ok(PyList::new_bound(py, &keys).into_any().unbind())
    }

    /// Return a string representation of this environment.
    fn __repr__(&self) -> String {
        format!("<Environment {}>", self.command)
    }

    /// Enter a `with` block, returning the built environment as a plain
    /// `dict` suitable for `subprocess.run(env=...)` and similar APIs:
    ///
    /// ```python
    /// with envoy.Environment("unreal") as env:
    ///     subprocess.run(["pyside6-uic", "ui.ui"], env=env)
    /// ```
    ///
    /// Returns a materialized `dict` (via `build`) rather than `self` so the
    /// yielded value works with any API expecting a plain mapping, without
    /// relying on duck-typing support for custom mapping objects.
    fn __enter__(&self, py: Python<'_>) -> PyResult<PyObject> {
        self.build(py)
    }

    /// Exit a `with` block.
    ///
    /// This environment has no external process state to revert on exit
    /// (unlike, say, temporarily patching `os.environ`), so this is a no-op
    /// that never suppresses exceptions raised in the `with` body.
    #[pyo3(signature = (_exc_type=None, _exc_value=None, _traceback=None))]
    fn __exit__(
        &self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_value: Option<&Bound<'_, PyAny>>,
        _traceback: Option<&Bound<'_, PyAny>>,
    ) -> bool {
        false
    }

    /// Build and return the environment as a dict (for compatibility).
    ///
    /// This method is provided for backward compatibility with code that
    /// expects a `.build()` step. In most cases, use dict-like access instead.
    fn build(&self, py: Python<'_>) -> PyResult<PyObject> {
        let env = self.get_env(py)?;
        let dict = PyDict::new_bound(py);
        for (key, value) in &env {
            dict.set_item(key, value)?;
        }
        Ok(dict.into_any().unbind())
    }

    /// Get the cached environment, building it if necessary.
    fn get_env(&self, _py: Python<'_>) -> PyResult<HashMap<String, String>> {
        let mut cached = self
            .cached
            .lock()
            .map_err(|e| PyValueError::new_err(format!("Failed to acquire lock: {}", e)))?;

        if let Some(cached_env) = cached.as_ref() {
            return Ok(cached_env.env.clone());
        }

        // Build the environment
        let built = build_environment(
            &self.command,
            self.inherit_env,
            self.allowlist.as_deref(),
            self.bundle_roots.as_deref(),
            self.commands_file.as_deref(),
        )?;

        *cached = Some(built.clone());
        Ok(built.env)
    }
}

/// Build the environment for a command.
fn build_environment(
    command: &str,
    inherit_env: bool,
    allowlist: Option<&[String]>,
    bundle_roots: Option<&[String]>,
    commands_file: Option<&std::path::Path>,
) -> PyResult<CachedEnv> {
    if is_raw_path(command) {
        // For raw paths, just use the parent environment with allowlist
        let mut env = HashMap::new();
        for (key, value) in std::env::vars() {
            if inherit_env || allowlist.is_some_and(|al| al.contains(&key)) {
                env.insert(key, value);
            }
        }

        return Ok(CachedEnv { env });
    }

    // Load registry and build environment through the standard path. The
    // environment is for running, so the bundles it needs come from the store.
    let (registry, bundles) = load_registry_for_run(bundle_roots, commands_file, command, None)
        .map_err(|e| PyValueError::new_err(format!("Failed to load registry: {}", e)))?;

    let (env, _command_definition) = prepare_env(
        command,
        &registry,
        bundles.as_deref(),
        inherit_env,
        allowlist,
        None, // env_override not supported in new API
    )
    .map_err(|e| PyValueError::new_err(format!("Failed to build environment: {}", e)))?;

    Ok(CachedEnv { env })
}

/// Convert a Python path-like object to a Rust PathBuf.
fn path_like_to_pathbuf(_py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<PathBuf> {
    if let Ok(s) = value.extract::<String>() {
        return Ok(PathBuf::from(s));
    }
    if let Ok(p) = value.downcast::<pyo3::types::PyString>() {
        let s = p
            .to_str()
            .map_err(|e| PyValueError::new_err(format!("Invalid UTF-8 in path: {}", e)))?;
        return Ok(PathBuf::from(s));
    }
    Err(PyValueError::new_err(
        "commands_file must be a string or path-like object",
    ))
}

/// Register the `envoy.Environment` class with the Python module.
pub fn register_environment_module(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let env_class = py.get_type_bound::<Environment>();
    parent.add("Environment", env_class)?;
    Ok(())
}
