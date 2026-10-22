mod app;
mod args;
mod cache_cmd;

pub fn run(argv: &[String]) -> i32 {
    app::run(argv)
}

#[cfg(test)]
mod tests {
    use super::run;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn run_returns_success_for_help_flag() {
        assert_eq!(run(&strings(&["--help"])), 0);
    }

    #[test]
    fn run_returns_success_for_version_flag() {
        assert_eq!(run(&strings(&["--version"])), 0);
    }
}
