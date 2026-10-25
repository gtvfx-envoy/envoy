//! Build script: embed the Windows icon and work out the version the shim reports.
//!
//! The version is built from the one `Cargo.toml` declares plus what git says
//! about the commit, as for `envoy-cli`; `../build_version.rs` describes how. The
//! target label is the one the release archives and the share's `shim/` folders use.

#[cfg(windows)]
use std::path::PathBuf;

#[path = "../build_version.rs"]
mod build_version;

fn main() {
    embed_windows_icon();

    let version = build_version::reported(env!("CARGO_PKG_VERSION"));
    println!("cargo:rustc-env=ENVOY_SHIM_VERSION={version}");
    println!("cargo:rustc-env=ENVOY_SHIM_TARGET={}", target_label());
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-changed=../build_version.rs");
}

/// `windows-x86_64`, `linux-x86_64-musl`, `macos-aarch64`, and so on.
fn target_label() -> String {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let abi = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "linux" && abi == "musl" {
        format!("{os}-{arch}-musl")
    } else {
        format!("{os}-{arch}")
    }
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
        .expect("failed to embed the Windows executable icon");
}

#[cfg(not(windows))]
fn embed_windows_icon() {}
