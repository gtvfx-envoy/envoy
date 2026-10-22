//! Options that belong to the shim rather than to envoy.
//!
//! They start with `--envoy-shim-` and are only recognised as the first
//! argument, so they can never be mistaken for something meant for envoy.
//!
//! | Option | What it does |
//! |---|---|
//! | `--envoy-shim-version` | Print the shim's version |
//! | `--envoy-shim-resolve` | Show which version this user gets, and why |
//! | `--envoy-shim-validate-config FILE` | Check a redirect file before it is published |
//! | `--envoy-shim-prefetch [VERSION]` | Cache a version now, for use offline later |
//! | `--envoy-shim-update` | Look for a newer shim on the share now, and install it |
//! | `--envoy-shim-help` | Show this list |

use std::ffi::OsString;
use std::fs;

use envoy_config::redirect::{is_safe_version, Redirect, Severity};
use envoy_store::{format_bytes, ProgressPrinter, Store};

use crate::app::{self, Found};
use crate::error::{EXIT_CONFIG, EXIT_USAGE};
use crate::source::Origin;
use crate::{decide, fail, how_long, update, VERSION};

/// What marks an argument as meant for the shim.
pub const PREFIX: &str = "--envoy-shim-";

const HELP: &str = "\
envoy shim options (only as the first argument; everything else goes to envoy):
  --envoy-shim-version                 print the shim's version
  --envoy-shim-resolve                 show which version this user gets, and why
  --envoy-shim-validate-config FILE    check a redirect file; add --check-sources
                                       to look for each version on the sources
  --envoy-shim-prefetch [VERSION]      cache a version now, for use offline
  --envoy-shim-update                  look for a newer shim on the share now
  --envoy-shim-help                    show this list";

/// Run the shim option `flag` with the arguments that follow it.
pub fn run(flag: &str, rest: &[OsString], env: &dyn Fn(&str) -> Option<String>) -> i32 {
    match flag {
        "--envoy-shim-version" => {
            println!("envoy-shim {VERSION}");
            0
        }
        "--envoy-shim-help" => {
            println!("{HELP}");
            0
        }
        "--envoy-shim-resolve" => resolve(env),
        "--envoy-shim-validate-config" => validate(rest, env),
        "--envoy-shim-prefetch" => prefetch(rest, env),
        "--envoy-shim-update" => update::run_now(env),
        _ => {
            eprintln!("envoy: unknown option {flag}; try --envoy-shim-help");
            EXIT_USAGE
        }
    }
}

fn resolve(env: &dyn Fn(&str) -> Option<String>) -> i32 {
    let decision = match decide(env) {
        Ok(decision) => decision,
        Err(error) => return fail(&error),
    };

    let user = if decision.user.is_empty() {
        "(unknown)"
    } else {
        &decision.user
    };
    println!("user:     {user}");
    let how = if decision.overridden {
        ""
    } else if decision.group == "default" {
        " (not listed, so the default)"
    } else {
        " (assigned)"
    };
    println!("group:    {}{how}", decision.group);
    println!("version:  {}", decision.version);

    match &decision.loaded.origin {
        Origin::Share(path) => println!("config:   {}", path.display()),
        Origin::Snapshot {
            source,
            age,
            reason,
        } => {
            println!(
                "config:   a copy of {}, saved {} ago ({reason})",
                source.display(),
                how_long(*age)
            );
        }
    }
    for key in &decision.loaded.redirect.unknown_keys {
        println!("warning:  the redirect file has an unknown key '{key}'");
    }

    println!(
        "cache:    {} ({})",
        decision.location.root.display(),
        if decision.location.enabled {
            "on".to_string()
        } else {
            format!("off, set by {}", decision.location.enabled_by)
        }
    );
    println!("launcher: {}", update::status(env));
    describe_app(&decision, env);
    0
}

/// Say where the pinned version is, or would be copied from.
fn describe_app(decision: &crate::Decision, env: &dyn Fn(&str) -> Option<String>) {
    let Ok(key) = app::key_for(&decision.version) else {
        return;
    };

    if decision.location.enabled {
        let cached = Store::open_existing(&decision.location.root)
            .ok()
            .flatten()
            .and_then(|store| {
                store
                    .lookup(&key)
                    .map(|state| (store.entry_path(&key), state))
            });
        if let Some((path, state)) = cached {
            println!(
                "app:      cached at {} ({})",
                path.display(),
                format_bytes(state.bytes)
            );
            return;
        }
    }

    match app::find_source(&decision.version, &decision.loaded.redirect.sources, env) {
        Ok(source) if decision.location.enabled => {
            println!(
                "app:      not cached yet; it would be copied from {}",
                source.display()
            );
        }
        Ok(source) => println!("app:      runs from {}", source.display()),
        Err(tried) if tried.is_empty() => println!("app:      the redirect file lists no sources"),
        Err(tried) => println!(
            "app:      not cached and not found. Looked in:\n  {}",
            tried.join("\n  ")
        ),
    }
}

fn validate(rest: &[OsString], env: &dyn Fn(&str) -> Option<String>) -> i32 {
    let mut file = None;
    let mut check_sources = false;
    for argument in rest {
        if argument == "--check-sources" {
            check_sources = true;
        } else if file.is_none() {
            file = Some(argument);
        } else {
            eprintln!("envoy: --envoy-shim-validate-config takes one file");
            return EXIT_USAGE;
        }
    }
    let Some(file) = file else {
        eprintln!("envoy: --envoy-shim-validate-config needs the file to check");
        return EXIT_USAGE;
    };

    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("envoy: cannot read {}: {error}", file.to_string_lossy());
            return EXIT_CONFIG;
        }
    };
    let redirect = match Redirect::parse(&text) {
        Ok(redirect) => redirect,
        Err(error) => {
            eprintln!("envoy: {}: {error}", file.to_string_lossy());
            return EXIT_CONFIG;
        }
    };

    let problems = redirect.validate();
    for problem in &problems {
        println!("{problem}");
    }
    let errors = problems
        .iter()
        .filter(|problem| problem.severity == Severity::Error)
        .count();

    if check_sources {
        let versions: std::collections::BTreeSet<&String> = redirect.versions.values().collect();
        for version in versions
            .into_iter()
            .filter(|version| is_safe_version(version))
        {
            if let Err(tried) = app::find_source(version, &redirect.sources, env) {
                println!(
                    "warning: version {version} was not found on any source:\n  {}",
                    tried.join("\n  ")
                );
            }
        }
    }

    if errors > 0 {
        eprintln!("envoy: {} has {errors} error(s)", file.to_string_lossy());
        return EXIT_CONFIG;
    }
    println!("{}: ok", file.to_string_lossy());
    0
}

fn prefetch(rest: &[OsString], env: &dyn Fn(&str) -> Option<String>) -> i32 {
    if rest.len() > 1 {
        eprintln!("envoy: --envoy-shim-prefetch takes at most one version");
        return EXIT_USAGE;
    }
    let decision = match decide(env) {
        Ok(decision) => decision,
        Err(error) => return fail(&error),
    };
    if !decision.location.enabled {
        println!(
            "Caching is off (set by {}), so there is nothing to prefetch.",
            decision.location.enabled_by
        );
        return 0;
    }

    let version = rest.first().map_or_else(
        || decision.version.clone(),
        |v| v.to_string_lossy().into_owned(),
    );
    if !is_safe_version(&version) {
        eprintln!("envoy: '{version}' is not a safe version name");
        return EXIT_USAGE;
    }

    match app::ensure(
        &version,
        &decision.loaded.redirect.sources,
        env,
        &decision.location,
        &ProgressPrinter::new(),
    ) {
        Ok(app) => {
            let from = match &app.found {
                Found::Cache(path) => format!("cached at {}", path.display()),
                Found::Source(path) => format!("running from {}", path.display()),
            };
            println!("envoy {version} is ready ({from})");
            0
        }
        Err(error) => fail(&error),
    }
}
