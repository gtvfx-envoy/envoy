#![allow(clippy::useless_conversion)]

//! PyO3 bindings for envoy's named-stack registry and user-config metadata.
//!
//! This module ports the public top-level surface historically re-exported
//! from `py/envoy/_stack_registry.py` and `py/envoy/_user_config.py`:
//! `NamedStackEntry`, `STACK_ROOTS_VAR`, `USER_CONFIG_PATH`,
//! `KNOWN_SETTINGS`, `getConfigRoot`, and the named-stack helper functions.

use std::path::Path;

use envoy_core::stack_registry::{
    is_stack_name as core_is_stack_name, list_named_stacks as core_list_named_stacks,
    list_stack_versions as core_list_stack_versions,
    resolve_named_stack as core_resolve_named_stack, NamedStackEntry as CoreNamedStackEntry,
    STACK_ROOTS_VAR,
};
use envoy_core::user_config::{config_root as core_config_root, known_settings, user_config_path};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule};

/// A single named stack entry discovered from ``ENVOY_STACK_ROOTS``.
///
/// Attributes:
///     name: The stack name (for example ``'studio'``).
///     version: The version timestamp directory name.
///     path: Absolute path to the resolved stack YAML file.
///     stack_root: Absolute path to the stack root directory that owns the
///         entry.
#[pyclass(module = "envoy")]
struct NamedStackEntry {
    inner: CoreNamedStackEntry,
}

#[pymethods]
impl NamedStackEntry {
    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    #[getter]
    fn version(&self) -> String {
        self.inner.version.clone()
    }

    #[getter]
    fn path(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, &self.inner.path)
    }

    #[getter]
    fn stack_root(&self, py: Python<'_>) -> PyResult<PyObject> {
        path_to_py_path(py, &self.inner.stack_root)
    }

    fn __repr__(&self) -> String {
        format!(
            "NamedStackEntry(name='{}', version='{}', path={}, stack_root={})",
            self.inner.name,
            self.inner.version,
            python_path_string(&self.inner.path),
            python_path_string(&self.inner.stack_root)
        )
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

impl NamedStackEntry {
    fn from_inner(inner: CoreNamedStackEntry) -> Self {
        Self { inner }
    }
}

/// Return ``True`` if ``value`` looks like a named stack rather than a path.
///
/// A value is treated as a name when it contains no path separator characters
/// (``/``, ``\\``, ``:``), does not start with a dot, and does not end in
/// ``.estack``. Everything else is treated as a filesystem path.
///
/// Args:
///     value: The raw string from ``stack`` or ``--stack``.
///
/// Returns:
///     ``True`` if ``value`` is a stack name; ``False`` if it looks like a
///     path.
///
/// Examples:
/// - Basic usage::
///     ```python
///     envoy.isStackName('studio')          # True
///     envoy.isStackName('my-stack')        # True
///     envoy.isStackName('/path/to/f.estack') # False
///     envoy.isStackName('R:/stacks/studio.estack') # False
///     envoy.isStackName('./relative.estack') # False
///     ```
#[pyfunction(name = "isStackName")]
fn is_stack_name(value: &str) -> bool {
    core_is_stack_name(value)
}

/// Resolve a named stack to the path of its latest version.
///
/// Searches each directory in ``ENVOY_STACK_ROOTS`` for a subdirectory named
/// ``name``. Envoy prefers a valid ``latest.estack`` symlink when present and
/// otherwise falls back to the published version whose ``.estack`` file has
/// the newest modified time. Returns the first match.
///
/// Args:
///     name: Stack name to resolve (for example ``'studio'``).
///
/// Returns:
///     Absolute path to the latest stack YAML file, or ``None`` if not found.
#[pyfunction(name = "resolveNamedStack")]
fn resolve_named_stack(py: Python<'_>, name: &str) -> PyResult<Option<PyObject>> {
    core_resolve_named_stack(name)
        .map(|path| path_to_py_path(py, &path))
        .transpose()
}

/// List all available named stacks across all ``ENVOY_STACK_ROOTS`` roots.
///
/// Scans each stack root for named subdirectories. For each name, envoy
/// prefers a valid ``latest.estack`` symlink when present and otherwise falls
/// back to the published version whose ``.estack`` file has the newest
/// modified time. Deduplicates by name — the first root that defines a given
/// name wins, matching :func:`resolveNamedStack`.
///
/// Returns:
///     List of :class:`NamedStackEntry` objects, sorted by name.
#[pyfunction(name = "listNamedStacks")]
fn list_named_stacks(py: Python<'_>) -> PyResult<Vec<Py<NamedStackEntry>>> {
    core_list_named_stacks()
        .into_iter()
        .map(|entry| Py::new(py, NamedStackEntry::from_inner(entry)))
        .collect()
}

/// List all published versions of a named stack, newest first.
///
/// Args:
///     name: Stack name (for example ``'studio'``).
///
/// Returns:
///     List of ``(version_string, absolute_path)`` tuples, newest first.
///     Returns an empty list if the name is not found in any root.
#[pyfunction(name = "listStackVersions")]
fn list_stack_versions(py: Python<'_>, name: &str) -> PyResult<Vec<(String, PyObject)>> {
    core_list_stack_versions(name)
        .into_iter()
        .map(|(version, path)| Ok((version, path_to_py_path(py, &path)?)))
        .collect()
}

/// Return Envoy's effective shared config root.
///
/// The result is ``$ENVOY_CONFIG_ROOT`` when that environment variable is
/// non-empty, otherwise ``~/.envoy``. The environment is checked on every
/// call, unlike the import-time ``USER_CONFIG_PATH`` compatibility constant.
///
/// Returns:
///     A :class:`pathlib.Path` for Envoy's effective config root.
#[pyfunction(name = "getConfigRoot")]
fn get_config_root(py: Python<'_>) -> PyResult<PyObject> {
    path_to_py_path(py, &core_config_root())
}

pub fn register_stack_registry_bindings(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NamedStackEntry>()?;
    m.add("STACK_ROOTS_VAR", STACK_ROOTS_VAR)?;
    m.add(
        "USER_CONFIG_PATH",
        path_to_py_path(py, &user_config_path())?,
    )?;
    m.add("KNOWN_SETTINGS", build_known_settings_dict(py)?)?;
    m.add_function(wrap_pyfunction!(get_config_root, m)?)?;
    m.add_function(wrap_pyfunction!(is_stack_name, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_named_stack, m)?)?;
    m.add_function(wrap_pyfunction!(list_named_stacks, m)?)?;
    m.add_function(wrap_pyfunction!(list_stack_versions, m)?)?;
    Ok(())
}

fn build_known_settings_dict(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let settings = PyDict::new_bound(py);

    for (name, setting) in known_settings() {
        let setting_dict = PyDict::new_bound(py);
        setting_dict.set_item("description", setting.description)?;

        match setting.choices {
            Some(choices) => setting_dict.set_item("choices", PyList::new_bound(py, choices))?,
            None => setting_dict.set_item("choices", py.None())?,
        }

        settings.set_item(name, setting_dict)?;
    }

    Ok(settings)
}

fn path_to_py_path(py: Python<'_>, path: &Path) -> PyResult<PyObject> {
    let pathlib = PyModule::import_bound(py, "pathlib")?;
    Ok(pathlib
        .getattr("Path")?
        .call1((python_path_string(path),))?
        .into_any()
        .unbind())
}

fn python_path_string(path: &Path) -> String {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(stripped) = text.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{stripped}");
        }
        if let Some(stripped) = text.strip_prefix(r"\\?\") {
            return stripped.to_string();
        }

        text.into_owned()
    }

    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{register_stack_registry_bindings, STACK_ROOTS_VAR};
    use pyo3::prelude::*;
    use pyo3::types::PyDict;
    use std::env;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{LazyLock, Mutex};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = env::var_os(name);
            env::set_var(name, value);

            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => env::set_var(self.name, value),
                None => env::remove_var(self.name),
            }
        }
    }

    fn with_python<T>(test_fn: impl FnOnce(Python<'_>) -> T) -> T {
        pyo3::prepare_freethreaded_python();
        Python::with_gil(test_fn)
    }

    #[test]
    fn known_settings_matches_python_shape() {
        with_python(|py| {
            let module = PyModule::new_bound(py, "envoy._envoy_test")
                .expect("test module should be created");
            register_stack_registry_bindings(py, &module)
                .expect("stack registry bindings should register");

            let settings_obj = module
                .getattr("KNOWN_SETTINGS")
                .expect("KNOWN_SETTINGS should exist");
            let settings = settings_obj
                .downcast::<PyDict>()
                .expect("KNOWN_SETTINGS should be a dict");

            let stack_obj = settings
                .get_item("stack")
                .expect("dict lookup should succeed")
                .expect("stack should exist");
            let stack = stack_obj
                .downcast::<PyDict>()
                .expect("stack entry should be a dict");
            let verbosity_obj = settings
                .get_item("verbosity")
                .expect("dict lookup should succeed")
                .expect("verbosity should exist");
            let verbosity = verbosity_obj
                .downcast::<PyDict>()
                .expect("verbosity entry should be a dict");

            let stack_choices = stack
                .get_item("choices")
                .expect("dict lookup should succeed")
                .expect("choices should exist");
            assert!(stack_choices.is_none());
            assert_eq!(
                verbosity
                    .get_item("choices")
                    .expect("dict lookup should succeed")
                    .expect("choices should exist")
                    .extract::<Vec<String>>()
                    .expect("choices should be a string list"),
                vec!["quiet", "normal", "verbose"]
            );
        });
    }

    #[test]
    fn config_root_function_is_dynamic_but_user_config_path_is_frozen() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let _guard = EnvVarGuard::set("ENVOY_CONFIG_ROOT", r"C:\envoy-tests\first");

        with_python(|py| {
            let module = PyModule::new_bound(py, "envoy._envoy_test")
                .expect("test module should be created");
            register_stack_registry_bindings(py, &module)
                .expect("stack registry bindings should register");

            env::set_var("ENVOY_CONFIG_ROOT", r"C:\envoy-tests\second");

            let frozen_path: String = module
                .getattr("USER_CONFIG_PATH")
                .expect("USER_CONFIG_PATH should exist")
                .call_method0("__fspath__")
                .expect("__fspath__ should exist")
                .extract()
                .expect("USER_CONFIG_PATH should be path-like");
            let stack_roots_var: String = module
                .getattr("STACK_ROOTS_VAR")
                .expect("STACK_ROOTS_VAR should exist")
                .extract()
                .expect("STACK_ROOTS_VAR should be str");
            let dynamic_root: String = module
                .getattr("getConfigRoot")
                .expect("getConfigRoot should exist")
                .call0()
                .expect("getConfigRoot should succeed")
                .call_method0("__fspath__")
                .expect("getConfigRoot result should be path-like")
                .extract()
                .expect("getConfigRoot result should be a string path");

            assert_eq!(stack_roots_var, STACK_ROOTS_VAR);
            assert_eq!(
                frozen_path,
                Path::new(r"C:\envoy-tests\first")
                    .join("user_config.json")
                    .to_string_lossy()
            );
            assert_eq!(dynamic_root, r"C:\envoy-tests\second");
        });
    }
}
