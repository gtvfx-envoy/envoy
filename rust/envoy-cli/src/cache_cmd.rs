//! `envoy cache ...` and `envoy --noop`: looking after the bundle cache.
//!
//! Everything here except warming works on the cache directory alone, so it
//! needs no stack, runs fast, and never creates a cache that is not there.

use std::time::SystemTime;

use envoy_core::bundle_store::{
    existing_store, format_bytes, last_prune, open_store, summarize, warm, warm_command,
    CacheSettings, Health, ProgressPrinter, PrunePolicy, WarmReport,
};
use envoy_core::commands::CommandRegistry;
use envoy_core::discovery::BundleInfo;

/// The actions `envoy cache` understands.
const ACTIONS: [&str; 6] = ["status", "list", "verify", "prune", "warm", "clear"];

/// The cache action that `command` and `args` ask for, if they ask for one.
///
/// `cache` is only taken as the cache command when an action follows it, so a
/// bundle's own `cache` command keeps working with any other argument.
pub(crate) fn action<'a>(command: Option<&str>, args: &'a [String]) -> Option<&'a str> {
    if command != Some("cache") {
        return None;
    }
    let first = args.first()?.as_str();
    ACTIONS.contains(&first).then_some(first)
}

/// Run a cache action that needs no stack: everything but `warm`.
pub(crate) fn run_action(action: &str, options: &[String], settings: &CacheSettings) -> i32 {
    let takes_dry_run = matches!(action, "prune" | "clear");
    let dry_run = match options {
        [] => false,
        [flag] if takes_dry_run && flag == "--dry-run" => true,
        _ => {
            eprintln!(
                "Error: unexpected arguments for 'envoy cache {action}': {}",
                options.join(" ")
            );
            return 1;
        }
    };

    match action {
        "status" => status(settings),
        "list" => list(settings),
        "verify" => verify(settings),
        "prune" => prune(settings, settings.prune_policy(), dry_run),
        "clear" => prune(settings, PrunePolicy::everything(), dry_run),
        _ => 1,
    }
}

fn status(settings: &CacheSettings) -> i32 {
    println!("Cache directory: {}", settings.root.display());
    println!(
        "Caching: {} (set by {})",
        if settings.enabled { "on" } else { "off" },
        settings.enabled_by
    );

    match summarize(settings) {
        Some(summary) => {
            println!(
                "Entries: {} ({})",
                summary.entries,
                format_bytes(summary.bytes)
            );
            if summary.unusable > 0 {
                println!(
                    "Unusable directories: {} (see 'envoy cache verify')",
                    summary.unusable
                );
            }
        }
        None => println!("Entries: none (the cache has not been created yet)"),
    }

    if let Some(store) = existing_store(settings) {
        let refused = store.refusals();
        if !refused.is_empty() {
            println!(
                "Bundles that do not fit in this directory: {}",
                refused.len()
            );
            for (key, error) in refused {
                println!("  {key}: {error}");
            }
        }
    }

    let size_cap = settings
        .max_bytes
        .map(|bytes| {
            format!(
                ", and the least recently used go beyond {}",
                format_bytes(bytes)
            )
        })
        .unwrap_or_default();
    println!(
        "Pruning: entries unused for {} days are removed{size_cap}",
        settings.max_idle.as_secs() / 86_400
    );
    match last_prune(settings) {
        Some(when) => println!("Last prune: {}", how_long_ago(SystemTime::now(), when)),
        None => println!("Last prune: never"),
    }
    0
}

fn list(settings: &CacheSettings) -> i32 {
    let entries = existing_store(settings)
        .map(|store| store.list())
        .unwrap_or_default();
    if entries.is_empty() {
        println!("Nothing is cached in {}.", settings.root.display());
        return 0;
    }

    let labels: Vec<String> = entries.iter().map(|entry| entry.key.to_string()).collect();
    let width = labels.iter().map(String::len).max().unwrap_or(0);
    let now = SystemTime::now();
    let mut total = 0;
    for (entry, label) in entries.iter().zip(&labels) {
        total += entry.bytes();
        let note = if !entry.usable() {
            "  (unusable)"
        } else if entry.keep {
            "  (kept)"
        } else {
            ""
        };
        println!(
            "{label:<width$}  {:>10}  last used {}{note}",
            format_bytes(entry.bytes()),
            how_long_ago(now, entry.last_used)
        );
    }
    println!(
        "{} {}, {}",
        entries.len(),
        plural(entries.len(), "entry", "entries"),
        format_bytes(total)
    );
    0
}

fn verify(settings: &CacheSettings) -> i32 {
    let Some(store) = existing_store(settings) else {
        println!(
            "Nothing to verify: {} has no cache.",
            settings.root.display()
        );
        return 0;
    };

    let verdicts = store.verify();
    let mut damaged = 0;
    for verdict in &verdicts {
        match &verdict.health {
            Health::Intact => println!("ok       {}", verdict.key),
            Health::Newer(format) => println!(
                "skipped  {} (written by a newer envoy, format {format})",
                verdict.key
            ),
            Health::Damaged(why) => {
                damaged += 1;
                println!("DAMAGED  {}: {why}", verdict.key);
            }
        }
    }

    if damaged == 0 {
        println!(
            "{} {} checked, none damaged.",
            verdicts.len(),
            plural(verdicts.len(), "entry", "entries")
        );
        return 0;
    }
    eprintln!(
        "{damaged} damaged. Delete a damaged entry's directory, or run 'envoy cache clear', \
and envoy copies it again the next time it is needed."
    );
    1
}

fn prune(settings: &CacheSettings, policy: PrunePolicy, dry_run: bool) -> i32 {
    let Some(store) = existing_store(settings) else {
        println!(
            "Nothing to remove: {} has no cache.",
            settings.root.display()
        );
        return 0;
    };

    let report = store.prune(&PrunePolicy { dry_run, ..policy });
    if report.removed.is_empty() {
        println!("Nothing to remove.");
    } else {
        println!(
            "{} {} {} ({}):",
            if dry_run { "Would remove" } else { "Removed" },
            report.removed.len(),
            plural(report.removed.len(), "entry", "entries"),
            format_bytes(report.freed_bytes)
        );
        for key in &report.removed {
            println!("  {key}");
        }
    }
    if !report.in_use.is_empty() {
        println!("Left because something is using them:");
        for key in &report.in_use {
            println!("  {key}");
        }
    }
    0
}

/// Copy bundles into the cache ahead of time: what `envoy --noop` and
/// `envoy cache warm` do.
///
/// With a `command`, only the bundles it needs are copied. Without one, every
/// published bundle in the stack is. The exit code is `1` when a bundle could
/// not be cached, so a scheduled warm-up shows up as failed.
pub(crate) fn warm_cache(
    registry: &CommandRegistry,
    bundles: Option<&[BundleInfo]>,
    command: Option<&str>,
    env_override: Option<&str>,
    settings: &CacheSettings,
) -> i32 {
    if !settings.enabled {
        println!(
            "Caching is off (set by {}), so there is nothing to warm.",
            settings.enabled_by
        );
        return 0;
    }
    let Some(bundles) = bundles else {
        eprintln!("Error: there are no bundles to cache");
        return 1;
    };
    if let Some(name) = command {
        if registry.get(name).is_none() {
            eprintln!("Error: Command '{name}' not found");
            eprintln!("Run 'envoy --list' to see available commands");
            return 1;
        }
    }
    let Some(store) = open_store(settings) else {
        return 1;
    };

    let printer = ProgressPrinter::new();
    let report = match command {
        Some(name) => warm_command(registry, bundles, name, env_override, &store, &printer),
        None => warm(bundles, &store, &printer),
    };

    print_warm_report(&report, settings);
    i32::from(!report.failed.is_empty())
}

fn print_warm_report(report: &WarmReport, settings: &CacheSettings) {
    println!("Cache directory: {}", settings.root.display());
    println!(
        "Copied {} ({}), {} already cached",
        count(report.filled.len(), "bundle", "bundles"),
        format_bytes(report.filled_bytes),
        report.already_cached
    );

    if !report.failed.is_empty() {
        println!(
            "Could not cache {} (they still run from where they are):",
            count(report.failed.len(), "bundle", "bundles")
        );
        for (label, reason) in &report.failed {
            println!("  {label}: {reason}");
        }
    }
    if !report.not_cacheable.is_empty() {
        println!(
            "Never cached: {}",
            count(report.not_cacheable.len(), "bundle", "bundles")
        );
        for (label, reason) in &report.not_cacheable {
            println!("  {label}: {reason}");
        }
    }
}

fn plural(number: usize, one: &'static str, many: &'static str) -> &'static str {
    if number == 1 {
        one
    } else {
        many
    }
}

fn count(number: usize, one: &'static str, many: &'static str) -> String {
    format!("{number} {}", plural(number, one, many))
}

fn how_long_ago(now: SystemTime, then: SystemTime) -> String {
    let seconds = now
        .duration_since(then)
        .map_or(0, |elapsed| elapsed.as_secs());
    let (number, unit) = match seconds {
        0..=59 => return "just now".to_string(),
        60..=3_599 => (seconds / 60, "minute"),
        3_600..=86_399 => (seconds / 3_600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    format!("{number} {unit}{} ago", if number == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn only_a_known_action_after_cache_is_a_cache_action() {
        assert_eq!(action(Some("cache"), &strings(&["status"])), Some("status"));
        assert_eq!(
            action(Some("cache"), &strings(&["warm", "maya"])),
            Some("warm")
        );
        assert_eq!(action(Some("cache"), &strings(&["something-else"])), None);
        assert_eq!(action(Some("cache"), &[]), None);
        assert_eq!(action(Some("maya"), &strings(&["status"])), None);
        assert_eq!(action(None, &strings(&["status"])), None);
    }

    #[test]
    fn ages_are_written_for_people() {
        let now = SystemTime::now();
        let ago = |seconds| how_long_ago(now, now - Duration::from_secs(seconds));

        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(2 * 3_600), "2 hours ago");
        assert_eq!(ago(31 * 86_400), "31 days ago");
    }
}
