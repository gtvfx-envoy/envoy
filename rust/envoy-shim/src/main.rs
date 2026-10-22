fn main() {
    std::process::exit(envoy_shim::run(std::env::args_os().skip(1).collect()));
}
