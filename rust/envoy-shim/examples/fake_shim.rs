//! A stand-in for a newer published launcher, used by the shim's update tests.
//!
//! It answers `--envoy-shim-version` with the version in `FAKE_SHIM_VERSION`
//! (`v9.9.9` when that is not set).

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--envoy-shim-version") {
        let version = std::env::var("FAKE_SHIM_VERSION").unwrap_or_else(|_| "v9.9.9".to_string());
        println!("envoy-shim {version}");
        return;
    }
    println!("the stand-in launcher ran");
}
