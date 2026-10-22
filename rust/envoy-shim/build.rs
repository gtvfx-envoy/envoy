//! Build script: embed the Windows icon and derive the shim's version string.
//!
//! The reported version comes from `git describe`, as for `envoy-cli`, and falls
//! back to the crate version when git is not available.

#[cfg(windows)]
use std::path::PathBuf;
use std::process::Command;

fn main() {
    embed_windows_icon();

    let version = git_describe().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=ENVOY_SHIM_VERSION={version}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
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

fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let version = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!version.is_empty()).then_some(version)
}
