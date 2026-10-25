//! Build script: embed the Windows icon and work out the version
//! `envoy --version` reports.
//!
//! The version is built from the one `Cargo.toml` declares plus what git says
//! about the commit; `../build_version.rs` describes how. It reaches the code as
//! the `ENVOY_VERSION` build-time variable, which `src/args.rs` hands to clap with
//! `#[command(version = env!("ENVOY_VERSION"))]`.

#[cfg(windows)]
use std::path::PathBuf;

#[path = "../build_version.rs"]
mod build_version;

fn main() {
    embed_windows_icon();

    let version = build_version::reported(env!("CARGO_PKG_VERSION"));
    println!("cargo:rustc-env=ENVOY_VERSION={version}");

    // Re-run if the git ref changes (new commit/tag), so `--version` output
    // stays current across incremental builds.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
    println!("cargo:rerun-if-changed=../build_version.rs");
}

#[cfg(windows)]
fn embed_windows_icon() {
    let icon_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources/icons/envoy.ico");
    println!("cargo:rerun-if-changed={}", icon_path.display());

    let icon_path = icon_path.to_string_lossy();
    let mut windows_resource = winres::WindowsResource::new();
    windows_resource.set_icon(&icon_path);
    windows_resource
        .compile()
        .expect("failed to embed the Envoy Windows executable icon");
}

#[cfg(not(windows))]
fn embed_windows_icon() {}
