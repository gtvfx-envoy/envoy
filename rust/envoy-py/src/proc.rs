#![allow(clippy::too_many_arguments, clippy::useless_conversion)]

//! PyO3 port of `py/envoy/proc.py`.
//!
//! This module preserves envoy's primary Python subprocess API:
//! - module-level `PIPE`, `STDOUT`, and `DEVNULL` constants
//! - free functions `call`, `spawn`, `checkCall`, and `checkOutput`
//! - the cached `Environment` launcher class
//! - Python-visible aliases for the canonical envoy exception types used by
//!   the process-launching API
//!
//! The free functions always invoke the envoy CLI executable resolved by
//! [`envoy_core::runtime::resolve_envoy_exe`]. The [`Environment`] class is
//! different: it builds command environments in-process via
//! [`envoy_core::runtime::prepare_env`] and launches the resolved executable
//! directly, matching the original Python module's split behavior.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::exceptions::{
    called_process_error, envoy_error_to_pyerr, CalledProcessError, CommandNotFoundError,
    EnvironmentBuildError,
};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use envoy_core::bundle_store::load_registry_for_run;
use envoy_core::commands::CommandDefinition;
use envoy_core::environment::EnvironmentManager;
use envoy_core::executor::ProcessExecutor;
use envoy_core::runtime::{is_raw_path, prepare_env, resolve_envoy_exe};
use pyo3::exceptions::{PyOSError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBytes, PyDict, PyModule, PyString};

const PIPE_SENTINEL: i32 = -1;
const STDOUT_SENTINEL: i32 = -2;
const DEVNULL_SENTINEL: i32 = -3;

#[cfg(windows)]
const CREATE_NO_WINDOW_FLAG: u32 = 0x0800_0000;

const PROC_MODULE_DOC: &str = r#"envoy.proc -- Process execution with pre-built command environments.

This module is the primary Python API for launching managed subprocesses
through envoy's environment system.
"#;

type SharedReader = Arc<Mutex<Option<BufReader<Box<dyn Read + Send>>>>>;
type SharedWriter = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamSpec {
    Inherit,
    Pipe,
    Null,
    Stdout,
}

#[derive(Debug, Default)]
struct SpawnOptions {
    stdin: Option<StreamSpec>,
    stdout: Option<StreamSpec>,
    stderr: Option<StreamSpec>,
    cwd: Option<PathBuf>,
    creationflags: Option<u32>,
    env: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug)]
struct CachedEnvironment {
    env: HashMap<String, String>,
    command_definition: CommandDefinition,
}

struct PopenState {
    child: Option<Child>,
    returncode: Option<i32>,
    stdin: Option<SharedWriter>,
    stdout: Option<SharedReader>,
    stderr: Option<SharedReader>,
    cached_stdout: Option<Vec<u8>>,
    cached_stderr: Option<Vec<u8>>,
}

#[pyclass(module = "envoy.proc")]
struct PyPipeReader {
    reader: SharedReader,
}

#[pymethods]
impl PyPipeReader {
    /// Read bytes from the captured pipe.
    ///
    /// Args:
    ///     size: Maximum number of bytes to read. When omitted or ``None``,
    ///         the method drains the remaining stream to EOF.
    ///
    /// Returns:
    ///     The bytes read from the pipe.
    #[pyo3(signature = (size=None))]
    fn read(&self, py: Python<'_>, size: Option<usize>) -> PyResult<Py<PyBytes>> {
        let mut guard = lock_mutex(&self.reader)?;
        let Some(reader) = guard.as_mut() else {
            return Ok(PyBytes::new_bound(py, &[]).into());
        };

        let data = match size {
            Some(limit) => {
                let mut buffer = vec![0_u8; limit];
                let read_count = reader.read(&mut buffer).map_err(io_to_pyerr)?;
                buffer.truncate(read_count);
                buffer
            }
            None => {
                let mut buffer = Vec::new();
                reader.read_to_end(&mut buffer).map_err(io_to_pyerr)?;
                buffer
            }
        };

        Ok(PyBytes::new_bound(py, &data).into())
    }

    /// Read one line from the captured pipe.
    ///
    /// Returns:
    ///     A single line of bytes, including the trailing newline when one
    ///     was present in the underlying stream.
    fn readline(&self, py: Python<'_>) -> PyResult<Py<PyBytes>> {
        let mut guard = lock_mutex(&self.reader)?;
        let Some(reader) = guard.as_mut() else {
            return Ok(PyBytes::new_bound(py, &[]).into());
        };

        let mut buffer = Vec::new();
        reader.read_until(b'\n', &mut buffer).map_err(io_to_pyerr)?;
        Ok(PyBytes::new_bound(py, &buffer).into())
    }

    /// Close the captured pipe reader.
    fn close(&self) -> PyResult<()> {
        *lock_mutex(&self.reader)? = None;
        Ok(())
    }
}

#[pyclass(module = "envoy.proc")]
struct PyPipeWriter {
    writer: SharedWriter,
}

#[pymethods]
impl PyPipeWriter {
    /// Write bytes or text to the process stdin pipe.
    ///
    /// Args:
    ///     data: Bytes or UTF-8 text to write.
    ///
    /// Returns:
    ///     Number of bytes written.
    fn write(&self, data: &Bound<'_, PyAny>) -> PyResult<usize> {
        let bytes = python_input_to_bytes(data)?;
        let mut guard = lock_mutex(&self.writer)?;
        let Some(writer) = guard.as_mut() else {
            return Ok(0);
        };

        writer.write_all(&bytes).map_err(io_to_pyerr)?;
        Ok(bytes.len())
    }

    /// Flush the stdin pipe.
    fn flush(&self) -> PyResult<()> {
        let mut guard = lock_mutex(&self.writer)?;
        if let Some(writer) = guard.as_mut() {
            writer.flush().map_err(io_to_pyerr)?;
        }
        Ok(())
    }

    /// Close the stdin pipe.
    fn close(&self) -> PyResult<()> {
        *lock_mutex(&self.writer)? = None;
        Ok(())
    }
}

#[pyclass(module = "envoy.proc")]
struct PyPopen {
    args: Vec<String>,
    pid: u32,
    state: Mutex<PopenState>,
    stdin_obj: Option<Py<PyPipeWriter>>,
    stdout_obj: Option<Py<PyPipeReader>>,
    stderr_obj: Option<Py<PyPipeReader>>,
}

#[pymethods]
impl PyPopen {
    /// Wait for the child process to exit.
    ///
    /// Args:
    ///     timeout: Optional timeout in seconds. When provided and the
    ///         process is still running after the timeout expires,
    ///         ``subprocess.TimeoutExpired`` is raised.
    ///
    /// Returns:
    ///     The process exit code.
    #[pyo3(signature = (timeout=None))]
    fn wait(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<i32> {
        let mut state = lock_mutex(&self.state)?;
        if let Some(returncode) = state.returncode {
            return Ok(returncode);
        }

        let Some(child) = state.child.as_mut() else {
            return Ok(state.returncode.unwrap_or_default());
        };

        let status = match timeout {
            Some(timeout_secs) => wait_with_timeout(py, child, timeout_secs, &self.args)?,
            None => child.wait().map_err(io_to_pyerr)?,
        };
        let returncode = exit_status_code(status);
        state.returncode = Some(returncode);
        Ok(returncode)
    }

    /// Read captured output streams to completion and wait for process exit.
    ///
    /// Args:
    ///     input: Optional bytes or text to send to the child's stdin. This
    ///         requires ``stdin=PIPE`` when the process was spawned.
    ///
    /// Returns:
    ///     ``(stdout, stderr)`` where each captured stream is returned as
    ///     bytes and uncaptured streams are returned as ``None``.
    #[pyo3(signature = (input=None))]
    fn communicate(
        &self,
        py: Python<'_>,
        input: Option<Py<PyAny>>,
    ) -> PyResult<(PyObject, PyObject)> {
        let input_bytes = input
            .as_ref()
            .map(|value| python_input_to_bytes(value.bind(py)))
            .transpose()?;

        let mut state = lock_mutex(&self.state)?;
        if state.child.is_none() {
            return cached_communicate_result(py, &state);
        }

        if input_bytes.is_some() && state.stdin.is_none() {
            return Err(PyValueError::new_err(
                "Cannot send input to a process that was not spawned with stdin=PIPE",
            ));
        }

        if let Some(stdin) = state.stdin.take() {
            if let Some(bytes) = input_bytes.as_deref() {
                write_to_shared_writer(&stdin, bytes)?;
            }
            close_shared_writer(&stdin)?;
        }

        let stdout_handle = state.stdout.clone().map(start_reader_thread);
        let stderr_handle = state.stderr.clone().map(start_reader_thread);

        let status = state
            .child
            .as_mut()
            .expect("child presence checked above")
            .wait()
            .map_err(io_to_pyerr)?;
        let returncode = exit_status_code(status);
        state.returncode = Some(returncode);
        state.child = None;

        let mut stdout = join_reader_thread(stdout_handle)?;
        let mut stderr = join_reader_thread(stderr_handle)?;

        if self.stderr_obj.is_none() && !stderr.is_empty() {
            stdout.extend_from_slice(&stderr);
            stderr.clear();
        }

        state.cached_stdout = Some(stdout.clone());
        state.cached_stderr = if self.stderr_obj.is_some() {
            Some(stderr.clone())
        } else {
            None
        };

        let stdout_obj = PyBytes::new_bound(py, &stdout).into_any().unbind();
        let stderr_obj = match self.stderr_obj {
            Some(_) => PyBytes::new_bound(py, &stderr).into_any().unbind(),
            None => py.None(),
        };

        Ok((stdout_obj, stderr_obj))
    }

    /// Return the child process ID.
    #[getter]
    fn pid(&self) -> u32 {
        self.pid
    }

    /// Return the child process exit code, or ``None`` while still running.
    #[getter]
    fn returncode(&self) -> PyResult<Option<i32>> {
        Ok(lock_mutex(&self.state)?.returncode)
    }

    /// Return the argv vector used to spawn the process.
    #[getter]
    fn args(&self) -> Vec<String> {
        self.args.clone()
    }

    /// Return the stdin pipe wrapper, when the process was spawned with
    /// ``stdin=PIPE``.
    #[getter]
    fn stdin(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.stdin_obj
            .as_ref()
            .map(|obj| obj.clone_ref(py).into_any())
    }

    /// Return the stdout pipe wrapper, when the process was spawned with
    /// ``stdout=PIPE``.
    #[getter]
    fn stdout(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.stdout_obj
            .as_ref()
            .map(|obj| obj.clone_ref(py).into_any())
    }

    /// Return the stderr pipe wrapper, when the process was spawned with
    /// ``stderr=PIPE``.
    #[getter]
    fn stderr(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.stderr_obj
            .as_ref()
            .map(|obj| obj.clone_ref(py).into_any())
    }

    fn __repr__(&self) -> PyResult<String> {
        let returncode = lock_mutex(&self.state)?.returncode;
        Ok(format!(
            "<PyPopen pid={} returncode={:?} args={:?}>",
            self.pid, returncode, self.args
        ))
    }
}

#[pyclass(module = "envoy.proc")]
struct Environment {
    command: String,
    inherit_env: bool,
    allowlist: Option<Vec<String>>,
    bundle_roots: Option<Vec<String>>,
    commands_file: Option<PathBuf>,
    env_override: Option<String>,
    cache: Mutex<Option<CachedEnvironment>>,
    // Caches the actual `dict` object returned by `build()`, separate from
    // `cache` (the parsed env-var map), so repeated `build()` calls return
    // the *same* Python object -- matching `py/envoy/proc.py`'s
    // `self._env` instance-attribute caching (`build()` is documented as
    // idempotent: "after the first call the cached result is returned").
    built_dict: Mutex<Option<Py<PyDict>>>,
}

#[pymethods]
impl Environment {
    /// Create a cached launcher for one envoy command environment.
    ///
    /// Bundle discovery and environment parsing happen at most once, on the
    /// first call to :meth:`build`, :meth:`spawn`, :meth:`call`,
    /// :meth:`checkCall`, or :meth:`checkOutput`.
    ///
    /// Args:
    ///     command: Envoy command name or raw executable path.
    ///     inherit_env: When ``False`` the child receives a closed
    ///         environment seeded only with envoy's core allowlisted
    ///         variables and the resolved env-file values.
    ///     allowlist: Additional variable names to seed in closed mode.
    ///     whitelist: Deprecated alias for ``allowlist``.
    ///     bundle_roots: Optional explicit bundle roots replacing
    ///         ``ENVOY_BNDL_ROOTS`` for this instance.
    ///     commands_file: Optional fallback ``commands.json`` path.
    ///     env_override: Optional registered command name whose env files
    ///         should be loaded instead of ``command``.
    #[new]
    #[pyo3(signature = (command, *, inherit_env=false, allowlist=None, whitelist=None, bundle_roots=None, commands_file=None, env_override=None))]
    fn new(
        py: Python<'_>,
        command: String,
        inherit_env: bool,
        allowlist: Option<Vec<String>>,
        whitelist: Option<Vec<String>>,
        bundle_roots: Option<Vec<String>>,
        commands_file: Option<&Bound<'_, PyAny>>,
        env_override: Option<String>,
    ) -> PyResult<Self> {
        let mut combined = allowlist.unwrap_or_default();
        combined.extend(whitelist.unwrap_or_default());

        Ok(Self {
            command,
            inherit_env,
            allowlist: (!combined.is_empty()).then_some(combined),
            bundle_roots,
            commands_file: commands_file
                .map(|value| path_like_to_pathbuf(py, value))
                .transpose()?,
            env_override,
            cache: Mutex::new(None),
            built_dict: Mutex::new(None),
        })
    }

    /// Return the command name or raw path this environment was created for.
    #[getter]
    fn command(&self) -> String {
        self.command.clone()
    }

    /// Return the merged allowlist passed at construction time.
    #[getter]
    fn allowlist(&self) -> Vec<String> {
        self.allowlist.clone().unwrap_or_default()
    }

    /// Return the deprecated ``whitelist`` alias.
    #[getter]
    fn whitelist(&self) -> Vec<String> {
        self.allowlist()
    }

    /// Build and cache the subprocess environment.
    ///
    /// Returns:
    ///     A dictionary containing the final child environment.
    ///
    /// Raises:
    ///     CommandNotFoundError: The requested command or env override is not
    ///         present in the discovered registry.
    ///     EnvironmentBuildError: Env files, command expansion, or executable
    ///         resolution failed.
    fn build(&self, py: Python<'_>) -> PyResult<PyObject> {
        let mut dict_guard = lock_mutex(&self.built_dict)?;
        if let Some(cached_dict) = dict_guard.as_ref() {
            return Ok(cached_dict.clone_ref(py).into_any());
        }

        let cached = self.ensure_cache()?;
        let dict = PyDict::new_bound(py);
        for (key, value) in &cached.env {
            dict.set_item(key, value)?;
        }
        let dict: Py<PyDict> = dict.unbind();
        *dict_guard = Some(dict.clone_ref(py));
        Ok(dict.into_any())
    }

    /// Launch the command asynchronously and return a Popen-like object.
    ///
    /// Args:
    ///     args: Extra arguments appended to the command's executable or
    ///         alias base arguments.
    ///     **kwargs: Subprocess-style options. Supported keys are ``stdin``,
    ///         ``stdout``, ``stderr``, ``cwd``, and ``creationflags``.
    ///
    /// Returns:
    ///     The running Popen-like object.
    #[pyo3(signature = (args=None, **kwargs))]
    fn spawn(
        &self,
        py: Python<'_>,
        args: Option<Vec<String>>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyPopen>> {
        let cached = self.ensure_cache()?;
        let extra_args = args.unwrap_or_default();
        let options = parse_spawn_options(py, kwargs, false)?;

        if is_raw_path(&self.command) && self.env_override.is_none() {
            return spawn_raw_command(py, &self.command, extra_args, &cached.env, options, true);
        }

        spawn_command_definition(
            py,
            &cached.command_definition,
            extra_args,
            &cached.env,
            options,
        )
    }

    /// Run the command synchronously and return its exit code.
    ///
    /// Raises:
    ///     ValueError: ``stdout`` or ``stderr`` was set to ``PIPE``.
    #[pyo3(signature = (args=None, **kwargs))]
    fn call(
        &self,
        py: Python<'_>,
        args: Option<Vec<String>>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<i32> {
        let options = parse_spawn_options(py, kwargs, false)?;
        validate_call_kwargs(&options)?;
        let proc = self.spawn(py, args, kwargs)?;
        proc.bind(py).borrow().wait(py, None)
    }

    /// Run the command and raise :class:`CalledProcessError` on failure.
    #[pyo3(name = "checkCall", signature = (args=None, **kwargs))]
    fn check_call(
        &self,
        py: Python<'_>,
        args: Option<Vec<String>>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<i32> {
        let returncode = self.call(py, args, kwargs)?;
        if returncode != 0 {
            let cached = self.ensure_cache()?;
            let executable = if is_raw_path(&self.command) && self.env_override.is_none() {
                self.command.clone()
            } else {
                cached.command_definition.executable().to_string()
            };
            return Err(called_process_error(py, returncode, executable, None, None));
        }
        Ok(returncode)
    }

    /// Run the command and capture stdout as bytes.
    ///
    /// ``stdout`` is always forced to ``PIPE``. ``stderr=STDOUT`` merges
    /// stderr into the returned stdout bytes using a post-process merge of
    /// the separately captured streams.
    #[pyo3(name = "checkOutput", signature = (args=None, **kwargs))]
    fn check_output(
        &self,
        py: Python<'_>,
        args: Option<Vec<String>>,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<PyBytes>> {
        let (options, input) = parse_check_output_options(py, kwargs, false)?;
        let proc = self.spawn_with_options(py, args.unwrap_or_default(), options)?;
        let (stdout_obj, stderr_obj) = proc.bind(py).borrow().communicate(py, input)?;
        let returncode = proc.bind(py).borrow().returncode()?.unwrap_or_default();
        let stdout = stdout_obj.bind(py).extract::<Vec<u8>>()?;
        let stderr = if stderr_obj.is_none(py) {
            None
        } else {
            Some(stderr_obj.bind(py).extract::<Vec<u8>>()?)
        };

        if returncode != 0 {
            let cached = self.ensure_cache()?;
            let executable = if is_raw_path(&self.command) && self.env_override.is_none() {
                self.command.clone()
            } else {
                cached.command_definition.executable().to_string()
            };
            return Err(called_process_error(
                py,
                returncode,
                executable,
                Some(stdout),
                stderr,
            ));
        }

        Ok(PyBytes::new_bound(py, &stdout).into())
    }

    fn __str__(&self) -> String {
        format!("<Environment {}>", self.command)
    }

    fn __repr__(&self) -> String {
        self.__str__()
    }
}

impl Environment {
    fn ensure_cache(&self) -> PyResult<CachedEnvironment> {
        let mut guard = lock_mutex(&self.cache)?;
        if let Some(cached) = guard.as_ref() {
            return Ok(cached.clone());
        }

        let cached = build_cached_environment(
            &self.command,
            self.inherit_env,
            self.allowlist.as_deref(),
            self.bundle_roots.as_deref(),
            self.commands_file.as_deref(),
            self.env_override.as_deref(),
        )?;
        *guard = Some(cached.clone());
        Ok(cached)
    }

    fn spawn_with_options(
        &self,
        py: Python<'_>,
        args: Vec<String>,
        options: SpawnOptions,
    ) -> PyResult<Py<PyPopen>> {
        let cached = self.ensure_cache()?;
        if is_raw_path(&self.command) && self.env_override.is_none() {
            return spawn_raw_command(py, &self.command, args, &cached.env, options, true);
        }

        spawn_command_definition(py, &cached.command_definition, args, &cached.env, options)
    }
}

/// Execute a command through the envoy CLI and return its exit code.
///
/// Args:
///     cmd: Full argv forwarded verbatim to the envoy CLI.
///     **kwargs: Subprocess-style options. Supported keys are ``stdin``,
///         ``stdout``, ``stderr``, ``cwd``, ``creationflags``, and ``env``.
///
/// Returns:
///     The process exit code.
///
/// Raises:
///     ValueError: ``cmd`` is empty or ``stdout``/``stderr`` is ``PIPE``.
#[pyfunction(signature = (cmd, **kwargs), name = "call")]
fn py_call(py: Python<'_>, cmd: Vec<String>, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<i32> {
    validate_cmd_not_empty(&cmd)?;
    let options = parse_spawn_options(py, kwargs, true)?;
    validate_call_kwargs(&options)?;
    let proc = spawn_cli_command(py, cmd, options)?;
    proc.bind(py).borrow().wait(py, None)
}

/// Execute a command through the envoy CLI and return immediately.
///
/// Args:
///     cmd: Full argv forwarded verbatim to the envoy CLI.
///     **kwargs: Subprocess-style options. Supported keys are ``stdin``,
///         ``stdout``, ``stderr``, ``cwd``, ``creationflags``, and ``env``.
///
/// Returns:
///     The running Popen-like object.
///
/// Raises:
///     ValueError: ``cmd`` is empty.
#[pyfunction(signature = (cmd, **kwargs), name = "spawn")]
fn py_spawn(
    py: Python<'_>,
    cmd: Vec<String>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyPopen>> {
    validate_cmd_not_empty(&cmd)?;
    let options = parse_spawn_options(py, kwargs, true)?;
    spawn_cli_command(py, cmd, options)
}

/// Execute a command through the envoy CLI and raise on non-zero exit.
#[pyfunction(signature = (cmd, **kwargs), name = "checkCall")]
fn py_check_call(
    py: Python<'_>,
    cmd: Vec<String>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<i32> {
    validate_cmd_not_empty(&cmd)?;
    let returncode = py_call(py, cmd.clone(), kwargs)?;
    if returncode != 0 {
        return Err(called_process_error(
            py,
            returncode,
            cmd[0].clone(),
            None,
            None,
        ));
    }
    Ok(returncode)
}

/// Execute a command through the envoy CLI and capture stdout as bytes.
#[pyfunction(signature = (cmd, **kwargs), name = "checkOutput")]
fn py_check_output(
    py: Python<'_>,
    cmd: Vec<String>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyBytes>> {
    validate_cmd_not_empty(&cmd)?;
    let (options, input) = parse_check_output_options(py, kwargs, true)?;
    let proc = spawn_cli_command(py, cmd.clone(), options)?;
    let (stdout_obj, stderr_obj) = proc.bind(py).borrow().communicate(py, input)?;
    let returncode = proc.bind(py).borrow().returncode()?.unwrap_or_default();
    let stdout = stdout_obj.bind(py).extract::<Vec<u8>>()?;
    let stderr = if stderr_obj.is_none(py) {
        None
    } else {
        Some(stderr_obj.bind(py).extract::<Vec<u8>>()?)
    };

    if returncode != 0 {
        return Err(called_process_error(
            py,
            returncode,
            cmd[0].clone(),
            Some(stdout),
            stderr,
        ));
    }

    Ok(PyBytes::new_bound(py, &stdout).into())
}

pub fn register_proc_module(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let module = PyModule::new_bound(py, "envoy.proc")?;
    module.add("__doc__", PROC_MODULE_DOC)?;
    module.add("PIPE", PIPE_SENTINEL)?;
    module.add("STDOUT", STDOUT_SENTINEL)?;
    module.add("DEVNULL", DEVNULL_SENTINEL)?;
    module.add(
        "CalledProcessError",
        py.get_type_bound::<CalledProcessError>(),
    )?;
    module.add(
        "CommandNotFoundError",
        py.get_type_bound::<CommandNotFoundError>(),
    )?;
    module.add(
        "EnvironmentBuildError",
        py.get_type_bound::<EnvironmentBuildError>(),
    )?;
    module.add_function(wrap_pyfunction!(py_call, &module)?)?;
    module.add_function(wrap_pyfunction!(py_spawn, &module)?)?;
    module.add_function(wrap_pyfunction!(py_check_call, &module)?)?;
    module.add_function(wrap_pyfunction!(py_check_output, &module)?)?;
    module.add_class::<Environment>()?;
    module.add_class::<PyPopen>()?;
    module.add_class::<PyPipeReader>()?;
    module.add_class::<PyPipeWriter>()?;
    parent.add("proc", module.clone())?;
    parent.add_submodule(&module)?;
    Ok(())
}

fn build_cached_environment(
    command: &str,
    inherit_env: bool,
    allowlist: Option<&[String]>,
    bundle_roots: Option<&[String]>,
    commands_file: Option<&Path>,
    env_override: Option<&str>,
) -> PyResult<CachedEnvironment> {
    if is_raw_path(command) && env_override.is_none() {
        let allowlist_set = allowlist
            .map(|items| items.iter().cloned().collect::<HashSet<_>>())
            .filter(|items| !items.is_empty());
        let env = EnvironmentManager::new(inherit_env, allowlist_set)
            .prepare_environment(&[], None, None, None)
            .map_err(envoy_error_to_pyerr)?;
        let command_definition = CommandDefinition {
            name: command.to_string(),
            environment: Vec::new(),
            alias: Some(vec![command.to_string()]),
            bundle: None,
            envoy_env_dir: None,
            source_file: None,
            platform_overrides: Vec::new(),
        };
        return Ok(CachedEnvironment {
            env,
            command_definition,
        });
    }

    let (registry, bundles) =
        load_registry_for_run(bundle_roots, commands_file, command, env_override)
            .map_err(envoy_error_to_pyerr)?;
    let (env, command_definition) = prepare_env(
        command,
        &registry,
        bundles.as_deref(),
        inherit_env,
        allowlist,
        env_override,
    )
    .map_err(envoy_error_to_pyerr)?;

    Ok(CachedEnvironment {
        env,
        command_definition,
    })
}

fn spawn_cli_command(
    py: Python<'_>,
    cmd: Vec<String>,
    mut options: SpawnOptions,
) -> PyResult<Py<PyPopen>> {
    let mut full_cmd = resolve_envoy_exe();
    full_cmd.extend(cmd);

    if let Some(search_env) = options.env.as_ref() {
        full_cmd[0] =
            resolve_command_for_spawn(&full_cmd[0], search_env.get("PATH").map(String::as_str))
                .map_err(io_to_pyerr)?;
    }

    spawn_process(py, full_cmd, options.env.take(), options, false)
}

fn spawn_raw_command(
    py: Python<'_>,
    executable: &str,
    extra_args: Vec<String>,
    env: &HashMap<String, String>,
    options: SpawnOptions,
    default_no_window: bool,
) -> PyResult<Py<PyPopen>> {
    let mut full_cmd = vec![executable.to_string()];
    full_cmd.extend(extra_args);

    #[cfg(windows)]
    let full_cmd = if Path::new(executable)
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("bat") || value.eq_ignore_ascii_case("cmd"))
    {
        let mut wrapped = vec![String::from("cmd"), String::from("/c")];
        wrapped.extend(full_cmd);
        wrapped
    } else {
        full_cmd
    };

    #[cfg(not(windows))]
    let full_cmd = full_cmd;

    spawn_process(
        py,
        full_cmd,
        Some(env.clone()),
        apply_default_creationflags(options, default_no_window),
        false,
    )
}

fn spawn_command_definition(
    py: Python<'_>,
    command_definition: &CommandDefinition,
    extra_args: Vec<String>,
    env: &HashMap<String, String>,
    options: SpawnOptions,
) -> PyResult<Py<PyPopen>> {
    let expanded = command_definition.expand_alias(Some(env));
    let executable = expanded
        .first()
        .cloned()
        .ok_or_else(|| EnvironmentBuildError::new_err("Command alias expanded to an empty argv"))?;
    let resolved = ProcessExecutor::resolve_executable(
        Path::new(&executable),
        env.get("PATH").map(String::as_str),
    )
    .map_err(envoy_error_to_pyerr)?;

    let mut full_cmd = vec![resolved.to_string_lossy().into_owned()];
    full_cmd.extend(expanded.into_iter().skip(1));
    full_cmd.extend(extra_args);

    #[cfg(windows)]
    let full_cmd = if resolved
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("bat") || value.eq_ignore_ascii_case("cmd"))
    {
        let mut wrapped = vec![String::from("cmd"), String::from("/c")];
        wrapped.extend(full_cmd);
        wrapped
    } else {
        full_cmd
    };

    #[cfg(not(windows))]
    let full_cmd = full_cmd;

    spawn_process(
        py,
        full_cmd,
        Some(env.clone()),
        apply_default_creationflags(options, true),
        true,
    )
}

#[cfg_attr(not(windows), allow(unused_mut, unused_variables))]
fn apply_default_creationflags(mut options: SpawnOptions, default_no_window: bool) -> SpawnOptions {
    #[cfg(windows)]
    if default_no_window && options.creationflags.is_none() {
        options.creationflags = Some(CREATE_NO_WINDOW_FLAG);
    }

    options
}

fn spawn_process(
    py: Python<'_>,
    argv: Vec<String>,
    env: Option<HashMap<String, String>>,
    options: SpawnOptions,
    _managed_environment: bool,
) -> PyResult<Py<PyPopen>> {
    let stdin_spec = options.stdin.unwrap_or(StreamSpec::Inherit);
    let stdout_spec = options.stdout.unwrap_or(StreamSpec::Inherit);
    let stderr_spec = options.stderr.unwrap_or(StreamSpec::Inherit);

    let merge_stderr_after_capture =
        stderr_spec == StreamSpec::Stdout && stdout_spec == StreamSpec::Pipe;
    let visible_stderr_pipe = stderr_spec == StreamSpec::Pipe;

    let mut command = Command::new(&argv[0]);
    if argv.len() > 1 {
        command.args(&argv[1..]);
    }
    if let Some(cwd) = options.cwd.as_ref() {
        command.current_dir(cwd);
    }
    if let Some(env_map) = env.as_ref() {
        command.env_clear();
        command.envs(env_map);
    }

    command.stdin(stdin_to_stdio(stdin_spec));
    command.stdout(stdout_to_stdio(stdout_spec));
    command.stderr(stderr_to_stdio(stderr_spec, stdout_spec));

    #[cfg(windows)]
    if let Some(creationflags) = options.creationflags {
        command.creation_flags(creationflags);
    }

    let mut child = command.spawn().map_err(io_to_pyerr)?;
    let pid = child.id();

    let stdin_state = child
        .stdin
        .take()
        .map(|pipe| Arc::new(Mutex::new(Some(Box::new(pipe) as Box<dyn Write + Send>))));
    let stdout_state = child.stdout.take().map(shared_reader_from_pipe);
    let stderr_state = child.stderr.take().map(shared_reader_from_pipe);

    let stdin_obj = stdin_state
        .clone()
        .map(|writer| Py::new(py, PyPipeWriter { writer }))
        .transpose()?;
    let stdout_obj = stdout_state
        .clone()
        .map(|reader| Py::new(py, PyPipeReader { reader }))
        .transpose()?;
    let stderr_obj = if visible_stderr_pipe {
        stderr_state
            .clone()
            .map(|reader| Py::new(py, PyPipeReader { reader }))
            .transpose()?
    } else {
        None
    };

    Py::new(
        py,
        PyPopen {
            args: argv,
            pid,
            state: Mutex::new(PopenState {
                child: Some(child),
                returncode: None,
                stdin: stdin_state,
                stdout: stdout_state,
                stderr: if merge_stderr_after_capture || visible_stderr_pipe {
                    stderr_state
                } else {
                    None
                },
                cached_stdout: None,
                cached_stderr: None,
            }),
            stdin_obj,
            stdout_obj,
            stderr_obj,
        },
    )
}

fn parse_spawn_options(
    py: Python<'_>,
    kwargs: Option<&Bound<'_, PyDict>>,
    allow_env_kwarg: bool,
) -> PyResult<SpawnOptions> {
    let mut options = SpawnOptions::default();
    let Some(kwargs) = kwargs else {
        return Ok(options);
    };

    for (key, value) in kwargs.iter() {
        let key = key.extract::<String>()?;
        match key.as_str() {
            "stdin" => options.stdin = Some(parse_stream_spec(&value, "stdin")?),
            "stdout" => options.stdout = Some(parse_stream_spec(&value, "stdout")?),
            "stderr" => options.stderr = Some(parse_stream_spec(&value, "stderr")?),
            "cwd" => options.cwd = Some(path_like_to_pathbuf(py, &value)?),
            "creationflags" => {
                let flags = value.extract::<u32>().map_err(|_| {
                    PyTypeError::new_err("'creationflags' must be a non-negative integer")
                })?;
                options.creationflags = Some(flags);
            }
            "env" if allow_env_kwarg => {
                options.env = Some(
                    value
                        .extract::<HashMap<String, String>>()
                        .map_err(|_| PyTypeError::new_err("'env' must be a dict[str, str]"))?,
                );
            }
            "env" => {
                return Err(PyTypeError::new_err(
                    "'env' is managed internally for Environment-spawned processes",
                ));
            }
            "input" => {}
            unsupported => {
                return Err(PyTypeError::new_err(format!(
                    "Unsupported keyword argument: '{unsupported}'"
                )));
            }
        }
    }

    Ok(options)
}

fn parse_check_output_options(
    py: Python<'_>,
    kwargs: Option<&Bound<'_, PyDict>>,
    allow_env_kwarg: bool,
) -> PyResult<(SpawnOptions, Option<Py<PyAny>>)> {
    let Some(kwargs) = kwargs else {
        let options = SpawnOptions {
            stdout: Some(StreamSpec::Pipe),
            ..SpawnOptions::default()
        };
        return Ok((options, None));
    };

    if kwargs.contains("stdout")? {
        return Err(PyValueError::new_err(
            "'stdout' argument not allowed in checkOutput; it will be overridden.",
        ));
    }

    let input = kwargs.get_item("input")?.map(|value| value.unbind());
    if input.is_some() && kwargs.contains("stdin")? {
        return Err(PyValueError::new_err(
            "'input' and 'stdin' cannot both be specified",
        ));
    }

    let mut options = parse_spawn_options(py, Some(kwargs), allow_env_kwarg)?;
    options.stdout = Some(StreamSpec::Pipe);
    if input.is_some() {
        options.stdin = Some(StreamSpec::Pipe);
    }

    Ok((options, input))
}

fn parse_stream_spec(value: &Bound<'_, PyAny>, stream_name: &str) -> PyResult<StreamSpec> {
    if value.is_none() {
        return Ok(StreamSpec::Inherit);
    }

    if let Ok(raw_value) = value.extract::<i32>() {
        return match raw_value {
            PIPE_SENTINEL => Ok(StreamSpec::Pipe),
            STDOUT_SENTINEL if stream_name == "stderr" => Ok(StreamSpec::Stdout),
            STDOUT_SENTINEL => Err(PyValueError::new_err(
                "'stdout' does not support STDOUT redirection",
            )),
            DEVNULL_SENTINEL => Ok(StreamSpec::Null),
            _ => Err(PyTypeError::new_err(format!(
                "'{stream_name}' must be envoy.proc.PIPE, envoy.proc.STDOUT, envoy.proc.DEVNULL, or None"
            ))),
        };
    }

    Err(PyTypeError::new_err(format!(
        "'{stream_name}' must be envoy.proc.PIPE, envoy.proc.STDOUT, envoy.proc.DEVNULL, or None"
    )))
}

fn validate_cmd_not_empty(cmd: &[String]) -> PyResult<()> {
    if cmd.is_empty() {
        Err(PyValueError::new_err("'cmd' must be a non-empty list"))
    } else {
        Ok(())
    }
}

fn validate_call_kwargs(options: &SpawnOptions) -> PyResult<()> {
    if options.stdout == Some(StreamSpec::Pipe) || options.stderr == Some(StreamSpec::Pipe) {
        return Err(PyValueError::new_err(
            "'call' does not support PIPE redirection for stdout/stderr. Use 'spawn' for async capture or 'checkOutput' for synchronous capture.",
        ));
    }
    Ok(())
}

fn stdin_to_stdio(spec: StreamSpec) -> Stdio {
    match spec {
        StreamSpec::Pipe => Stdio::piped(),
        StreamSpec::Null => Stdio::null(),
        StreamSpec::Inherit | StreamSpec::Stdout => Stdio::inherit(),
    }
}

fn stdout_to_stdio(spec: StreamSpec) -> Stdio {
    match spec {
        StreamSpec::Pipe => Stdio::piped(),
        StreamSpec::Null => Stdio::null(),
        StreamSpec::Inherit | StreamSpec::Stdout => Stdio::inherit(),
    }
}

fn stderr_to_stdio(stderr_spec: StreamSpec, stdout_spec: StreamSpec) -> Stdio {
    match stderr_spec {
        StreamSpec::Pipe => Stdio::piped(),
        StreamSpec::Null => Stdio::null(),
        StreamSpec::Stdout => match stdout_spec {
            StreamSpec::Pipe => Stdio::piped(),
            StreamSpec::Null => Stdio::null(),
            StreamSpec::Inherit | StreamSpec::Stdout => Stdio::inherit(),
        },
        StreamSpec::Inherit => Stdio::inherit(),
    }
}

fn shared_reader_from_pipe<T>(pipe: T) -> SharedReader
where
    T: Read + Send + 'static,
{
    Arc::new(Mutex::new(Some(BufReader::new(
        Box::new(pipe) as Box<dyn Read + Send>
    ))))
}

fn start_reader_thread(reader: SharedReader) -> thread::JoinHandle<std::io::Result<Vec<u8>>> {
    thread::spawn(move || read_all_from_shared_reader(&reader))
}

fn join_reader_thread(
    handle: Option<thread::JoinHandle<std::io::Result<Vec<u8>>>>,
) -> PyResult<Vec<u8>> {
    match handle {
        Some(handle) => handle
            .join()
            .map_err(|_| PyOSError::new_err("Reader thread panicked"))?
            .map_err(io_to_pyerr),
        None => Ok(Vec::new()),
    }
}

fn read_all_from_shared_reader(reader: &SharedReader) -> std::io::Result<Vec<u8>> {
    let mut guard = reader
        .lock()
        .map_err(|_| std::io::Error::other("reader mutex poisoned"))?;
    let Some(reader) = guard.as_mut() else {
        return Ok(Vec::new());
    };

    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn write_to_shared_writer(writer: &SharedWriter, bytes: &[u8]) -> PyResult<()> {
    let mut guard = lock_mutex(writer)?;
    let Some(writer) = guard.as_mut() else {
        return Ok(());
    };

    writer.write_all(bytes).map_err(io_to_pyerr)?;
    writer.flush().map_err(io_to_pyerr)?;
    Ok(())
}

fn close_shared_writer(writer: &SharedWriter) -> PyResult<()> {
    *lock_mutex(writer)? = None;
    Ok(())
}

fn cached_communicate_result(py: Python<'_>, state: &PopenState) -> PyResult<(PyObject, PyObject)> {
    let stdout = state.cached_stdout.clone().unwrap_or_default();
    let stdout_obj = PyBytes::new_bound(py, &stdout).into_any().unbind();
    let stderr_obj = match state.cached_stderr.as_ref() {
        Some(stderr) => PyBytes::new_bound(py, stderr).into_any().unbind(),
        None => py.None(),
    };
    Ok((stdout_obj, stderr_obj))
}

fn wait_with_timeout(
    py: Python<'_>,
    child: &mut Child,
    timeout_secs: f64,
    args: &[String],
) -> PyResult<ExitStatus> {
    if timeout_secs.is_sign_negative() {
        return Err(PyValueError::new_err("'timeout' must be non-negative"));
    }

    let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs);
    loop {
        match child.try_wait().map_err(io_to_pyerr)? {
            Some(status) => return Ok(status),
            None if Instant::now() >= deadline => {
                return Err(timeout_expired(py, args, timeout_secs));
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn timeout_expired(py: Python<'_>, args: &[String], timeout_secs: f64) -> PyErr {
    let subprocess =
        PyModule::import_bound(py, "subprocess").expect("subprocess module should be importable");
    let timeout_type = subprocess
        .getattr("TimeoutExpired")
        .expect("subprocess.TimeoutExpired should exist");
    let cmd = args.join(" ");
    let instance = timeout_type
        .call1((cmd.clone(), timeout_secs))
        .expect("TimeoutExpired constructor should succeed");
    instance
        .setattr("cmd", cmd)
        .expect("TimeoutExpired.cmd should be assignable");
    instance
        .setattr("timeout", timeout_secs)
        .expect("TimeoutExpired.timeout should be assignable");
    PyErr::from_value_bound(instance.into_any())
}

fn path_like_to_pathbuf(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<PathBuf> {
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

fn python_input_to_bytes(value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(bytes) = value.downcast::<PyBytes>() {
        return Ok(bytes.as_bytes().to_vec());
    }
    if let Ok(text) = value.downcast::<PyString>() {
        return Ok(text.to_string_lossy().as_bytes().to_vec());
    }
    value
        .extract::<Vec<u8>>()
        .map_err(|_| PyTypeError::new_err("'input' must be bytes or str"))
}

fn resolve_command_for_spawn(command: &str, search_path: Option<&str>) -> std::io::Result<String> {
    ProcessExecutor::resolve_executable(Path::new(command), search_path)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| std::io::Error::other(error.to_string()))
}

fn io_to_pyerr(error: std::io::Error) -> PyErr {
    PyOSError::new_err(error.to_string())
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> PyResult<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| PyOSError::new_err("internal mutex poisoned"))
}

#[cfg(unix)]
fn exit_status_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .unwrap_or_else(|| status.signal().map(|signal| -signal).unwrap_or(-1))
}

#[cfg(not(unix))]
fn exit_status_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::{
        parse_stream_spec, validate_call_kwargs, Environment, SpawnOptions, StreamSpec,
        DEVNULL_SENTINEL, PIPE_SENTINEL, STDOUT_SENTINEL,
    };
    use pyo3::types::PyAnyMethods;
    use pyo3::{IntoPy, Python};

    fn with_python<T>(test_fn: impl FnOnce(Python<'_>) -> T) -> T {
        pyo3::prepare_freethreaded_python();
        Python::with_gil(test_fn)
    }

    #[test]
    fn build_returns_same_python_dict_object_on_repeated_calls() {
        with_python(|py| {
            // A raw path (contains '/') bypasses registry/bundle discovery
            // entirely, so this test needs no filesystem fixtures.
            let env = Environment::new(py, "a/b".to_string(), false, None, None, None, None, None)
                .expect("Environment should construct");

            let first = env.build(py).expect("first build() should succeed");
            let second = env.build(py).expect("second build() should succeed");

            // Mirrors py/envoy/proc.py's Environment.build(), documented as
            // idempotent: "after the first call the cached result is
            // returned" -- i.e. the *same* dict object, not just an equal one.
            assert!(
                first.bind(py).is(second.bind(py)),
                "build() should return the same cached dict object on repeated calls"
            );
        });
    }

    #[test]
    fn call_validation_rejects_pipe_redirection() {
        let options = SpawnOptions {
            stdout: Some(StreamSpec::Pipe),
            ..SpawnOptions::default()
        };

        assert!(validate_call_kwargs(&options).is_err());
    }

    #[test]
    fn stream_spec_accepts_subprocess_sentinals() {
        with_python(|py| {
            let pipe = PIPE_SENTINEL.into_py(py);
            let stdout = STDOUT_SENTINEL.into_py(py);
            let devnull = DEVNULL_SENTINEL.into_py(py);

            assert_eq!(
                parse_stream_spec(pipe.bind(py), "stdout").expect("PIPE should parse"),
                StreamSpec::Pipe
            );
            assert_eq!(
                parse_stream_spec(stdout.bind(py), "stderr")
                    .expect("STDOUT should parse for stderr"),
                StreamSpec::Stdout
            );
            assert_eq!(
                parse_stream_spec(devnull.bind(py), "stdout").expect("DEVNULL should parse"),
                StreamSpec::Null
            );
        });
    }

    #[test]
    fn stream_spec_rejects_stdout_stdouterr_alias() {
        with_python(|py| {
            let stdout = STDOUT_SENTINEL.into_py(py);
            assert!(parse_stream_spec(stdout.bind(py), "stdout").is_err());
        });
    }
}
