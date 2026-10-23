//! Showing what a fill is doing on standard error.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::key::EntryKey;
use crate::observer::Observer;

/// How often a non-interactive run reports progress.
const LINE_PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// Shows what the store is doing on standard error.
///
/// On a terminal, progress rewrites one line. Otherwise a line is printed
/// every few seconds, so a log stays readable.
pub struct ProgressPrinter {
    interactive: bool,
    last_line: Mutex<Option<Instant>>,
    drew: AtomicBool,
}

impl ProgressPrinter {
    /// A printer for standard error as it is attached right now.
    pub fn new() -> Self {
        Self {
            interactive: std::io::stderr().is_terminal(),
            last_line: Mutex::new(None),
            drew: AtomicBool::new(false),
        }
    }
}

impl Default for ProgressPrinter {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer for ProgressPrinter {
    fn waiting(&self, key: &EntryKey) {
        eprintln!("envoy: waiting for another envoy to finish caching {key}...");
    }

    fn started(&self, key: &EntryKey, files: u64, bytes: u64) {
        eprintln!(
            "envoy: caching {key} ({files} files, {}) on first use...",
            format_bytes(bytes)
        );
    }

    fn progress(&self, key: &EntryKey, done: u64, total: u64) {
        let percent = (done * 100).checked_div(total).unwrap_or(100);
        if self.interactive {
            eprint!(
                "\r  {key}: {percent}% ({} of {})   ",
                format_bytes(done),
                format_bytes(total)
            );
            let _ = std::io::stderr().flush();
            self.drew.store(true, Ordering::Relaxed);
            return;
        }

        let mut last = self
            .last_line
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if last.is_none_or(|printed| printed.elapsed() >= LINE_PROGRESS_INTERVAL) {
            eprintln!("  {key}: {percent}%");
            *last = Some(Instant::now());
        }
    }

    fn finished(&self, _key: &EntryKey, _elapsed: Duration) {
        if self.drew.swap(false, Ordering::Relaxed) {
            eprintln!();
        }
    }
}

/// Format a byte count for people, such as `637.2 MB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_formatted_for_people() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(668_150_000), "637.2 MB");
    }
}
