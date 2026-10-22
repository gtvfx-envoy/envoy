//! Configuration files for envoy delivery.
//!
//! The **redirect** file pins which envoy version each user runs. A small shim
//! reads it before the app starts, caches the pinned version locally, and
//! launches it, so one file on the share controls the version for a whole team.
//!
//! Everything here is pure or touches only the files it is given, so the shim
//! stays light and the rules can be tested without a share:
//!
//! - [`redirect`] parses and validates the file and resolves a user to a version.
//! - [`expand`] turns a source template into a path.
//! - [`locate`] says where the file may be found.
//! - [`snapshot`] keeps the last good copy, so envoy still starts when the
//!   share is unreachable.
//!
//! ```
//! use envoy_config::redirect::Redirect;
//!
//! let redirect = Redirect::parse(
//!     "schema: 1\n\
//!      sources: ['/share/envoy/{version}']\n\
//!      versions: {default: v0.7.0, preview: v0.8.0-rc.1}\n\
//!      users: {alice: preview}\n",
//! )
//! .unwrap();
//!
//! assert_eq!(redirect.resolve("ALICE").unwrap().version, "v0.8.0-rc.1");
//! assert_eq!(redirect.resolve("bob").unwrap().version, "v0.7.0");
//! ```

pub mod error;
pub mod expand;
pub mod locate;
pub mod redirect;
pub mod snapshot;

pub use error::{ConfigError, Result};
pub use redirect::{normalize_user, Problem, Redirect, Resolution, Severity, SUPPORTED_SCHEMA};
