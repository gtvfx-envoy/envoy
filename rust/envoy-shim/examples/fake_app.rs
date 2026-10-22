//! A stand-in for the envoy app, used by the shim's tests.
//!
//! It reports how it was started, then exits with the code in `FAKE_APP_EXIT`.

fn main() {
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
