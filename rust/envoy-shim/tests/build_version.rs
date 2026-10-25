//! Runs the tests of the version logic that the build scripts of `envoy-cli`,
//! `envoy-shim` and `envoy-py` share, which build scripts cannot run themselves.

#[path = "../../build_version.rs"]
mod build_version;
