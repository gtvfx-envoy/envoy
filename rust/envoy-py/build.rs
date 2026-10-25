//! Build script: work out the version `envoy-py` reports.
//!
//! This mirrors `rust/envoy-cli/build.rs`: the version is built from the one
//! `Cargo.toml` declares plus what git says about the commit (see
//! `../build_version.rs`). It is exposed as `envoy._envoy.__version__` (see
//! `src/lib.rs`), which `python/envoy/__init__.py` re-exports as
//! `envoy.__version__`.

#[path = "../build_version.rs"]
mod build_version;

fn main() {
    let version = build_version::reported(env!("CARGO_PKG_VERSION"));
    println!("cargo:rustc-env=ENVOY_PY_VERSION={version}");

    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
    println!("cargo:rerun-if-changed=../build_version.rs");
}
