//! `envoy-core` -- pure Rust port of envoy's framework-agnostic logic.
//!
//! This crate contains no Python or CLI-specific code. It is consumed by:
//! - `envoy-py` (PyO3 bindings, preserving the `import envoy` Python API)
//! - `envoy-cli` (native CLI binary, no Python runtime dependency)
//! - `engit-core` (bundle discovery / named-config resolution for `engit`)
//!
//! Modules are ported from the original Python implementation in
//! `py/envoy/`, module-for-module, to keep behavior traceable during the
//! migration:
//!
//! | Rust module         | Python source           |
//! |----------------------|--------------------------|
//! | [`error`]            | `_exceptions.py`         |
//! | [`models`]           | `_models.py`             |
//! | [`user_config`]      | `_user_config.py`        |
//! | [`stack_registry`]   | named stack registry     |
//! | [`discovery`]        | `_discovery.py`          |
//! | [`environment`]      | `_environment.py`        |
//! | [`commands`]         | `_commands.py`           |
//! | [`executor`]         | `_executor.py`           |
//! | [`wrapper`]          | `_wrapper.py`            |

pub mod bundle_cache;
pub mod bundle_store;
pub mod commands;
pub mod config_crypto;
pub mod discovery;
pub mod environment;
pub mod error;
pub mod executor;
pub mod fs_lock;
pub mod json_util;
pub mod models;
pub mod python_api;
pub mod retry;
pub mod runtime;
pub mod semver;
pub mod stack;
pub mod stack_registry;
pub mod team_config;
pub mod telemetry;
pub mod user_config;
pub mod vcs;
pub mod wrapper;

pub use error::{EnvoyError, Result};

/// Shared test-only synchronization for tests that mutate real process
/// environment variables (`ENVOY_STACK_ROOTS`, `ENVOY_CONFIG_ROOT`,
/// `ENVOY_COMMANDS_FILE`, `ENVOY_BNDL_ROOTS`, etc.).
///
/// `commands`, `stack_registry`, `discovery`, and `environment` each have
/// tests that temporarily set/restore real environment variables. Since
/// `cargo test` runs tests in parallel threads within a single process, and
/// environment variables are process-global, tests in *different* modules
/// that each guarded their own module-local mutex could still race against
/// each other on the same real env var (e.g. both `discovery` and
/// `stack_registry` touch `ENVOY_STACK_ROOTS`). All such tests must lock this
/// single crate-wide mutex instead of a module-local one.
#[cfg(test)]
pub(crate) mod env_test_lock {
    use std::sync::Mutex;

    /// Crate-wide mutex guarding any test that mutates real environment
    /// variables. Lock this (via `.lock().unwrap_or_else(|poison|
    /// poison.into_inner())`) before mutating `std::env` in a test.
    pub(crate) static MUTEX: Mutex<()> = Mutex::new(());
}

#[cfg(test)]
pub(crate) mod path_test {
    use std::fs;
    use std::path::Path;

    pub(crate) fn assert_same_path(actual: &Path, expected: &Path) {
        let actual = fs::canonicalize(actual).expect("actual path should canonicalize");
        let expected = fs::canonicalize(expected).expect("expected path should canonicalize");
        assert_eq!(actual, expected);
    }
}
