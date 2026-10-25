//! A stand-in for the envoy app, used by the shim's tests.
//!
//! It reports how it was started, then exits with the code in `FAKE_APP_EXIT`.
//! Asked only for `--version` it answers as the real app does, with the name of
//! the folder it was published in, which is the version asked for, or with
//! `FAKE_APP_VERSION`, for a test of an app that names another version.

fn main() {
    if std::env::args().skip(1).collect::<Vec<_>>() == ["--version"] {
        println!("envoy {}", reported_version());
        return;
    }

    let exe = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    println!("exe={exe}");

    for argument in std::env::args().skip(1) {
        println!("arg={argument}");
    }
    for name in ["ENVOY_BIN_LOCK", "ENVOY_BIN_FLOAT"] {
        println!("{name}={}", std::env::var(name).unwrap_or_default());
    }

    let code = std::env::var("FAKE_APP_EXIT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    std::process::exit(code);
}

/// The folder the app sits in, `<version>/bin/envoy`, unless a test says otherwise.
fn reported_version() -> String {
    if let Ok(version) = std::env::var("FAKE_APP_VERSION") {
        return version;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            let folder = exe.parent()?.parent()?;
            Some(folder.file_name()?.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}
