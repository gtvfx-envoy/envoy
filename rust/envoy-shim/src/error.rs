use std::fmt;

/// Exit code for a shim option that was used wrongly (`EX_USAGE`).
pub const EXIT_USAGE: i32 = 64;

/// Exit code when the pinned version cannot be found or fetched (`EX_UNAVAILABLE`).
pub const EXIT_UNAVAILABLE: i32 = 69;

/// Exit code for a failure that should not happen (`EX_SOFTWARE`).
pub const EXIT_SOFTWARE: i32 = 70;

/// Exit code when the redirect file cannot be found or used (`EX_CONFIG`).
pub const EXIT_CONFIG: i32 = 78;

/// Why the shim could not start envoy.
///
/// The shim's own exit codes use the `sysexits.h` values above, which an envoy
/// run does not normally return, so a failure here is easy to tell from a
/// failure in envoy itself.
#[derive(Debug)]
pub enum ShimError {
    /// The redirect file, or a saved copy of it, could not be used.
    Redirect(String),
    /// The pinned version is neither cached nor reachable.
    Unavailable(String),
    /// Something went wrong that should not have.
    Internal(String),
}

impl ShimError {
    /// The exit code to end the process with.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Redirect(_) => EXIT_CONFIG,
            Self::Unavailable(_) => EXIT_UNAVAILABLE,
            Self::Internal(_) => EXIT_SOFTWARE,
        }
    }
}

impl fmt::Display for ShimError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redirect(message) | Self::Unavailable(message) | Self::Internal(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for ShimError {}

impl From<envoy_config::ConfigError> for ShimError {
    fn from(error: envoy_config::ConfigError) -> Self {
        Self::Redirect(error.to_string())
    }
}
