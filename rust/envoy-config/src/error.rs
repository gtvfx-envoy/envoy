use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Errors raised while reading or applying a configuration file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file is not valid, or does not have the shape expected.
    #[error("the redirect file is not valid: {0}")]
    Syntax(String),

    /// The file was written for a newer envoy.
    #[error(
        "the redirect file uses schema {found}, but this envoy understands up to schema \
{supported}; update envoy"
    )]
    NewerSchema { found: u32, supported: u32 },

    /// There is no version to give a user who has no assignment.
    #[error("the redirect file has no 'default' version, and '{user}' has no assignment")]
    NoDefault { user: String },

    /// A user is assigned to a group that has no version.
    #[error("'{user}' is assigned to group '{group}', which has no version")]
    UnknownGroup { user: String, group: String },

    /// A version is not a safe directory name, so it is never used.
    #[error("'{version}' is not a safe version name")]
    UnsafeVersion { version: String },

    /// A source template could not be turned into a path.
    #[error("cannot expand '{template}': {reason}")]
    Expand { template: String, reason: String },

    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Result alias for configuration operations.
pub type Result<T> = std::result::Result<T, ConfigError>;
