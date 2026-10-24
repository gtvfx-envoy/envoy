//! Showing what a fill is doing on standard error.

use std::io::{IsTerminal, Write};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::key::EntryKey;
use crate::observer::Observer;

/// How often a non-interactive run reports progress.
const LINE_PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// Shows what the store is doing on standard error.
///
/// Bundles are copied several at a time, so everything is written under one
/// lock. On a terminal one line, kept below the messages, shows how far the
/// copies in flight have got; it is wiped before anything else is printed, so a
/// message never lands inside it. Otherwise each copy in flight gets a line every
/// few seconds, so a log stays readable.
pub struct ProgressPrinter {
    screen: Mutex<Screen>,
}

struct Screen {
    out: Box<dyn Write + Send>,
    interactive: bool,
    /// The copies in flight, in the order they started.
    active: Vec<Active>,
    /// Width of the progress line on screen now, or 0 when there is none.
    drawn: usize,
    /// When a non-interactive run last reported progress.
    last_report: Option<Instant>,
}

struct Active {
    key: String,
    done: u64,
    total: u64,
}

impl ProgressPrinter {
    /// A printer for standard error as it is attached right now.
    pub fn new() -> Self {
        Self::writing_to(Box::new(std::io::stderr()), std::io::stderr().is_terminal())
    }

    fn writing_to(out: Box<dyn Write + Send>, interactive: bool) -> Self {
        Self {
            screen: Mutex::new(Screen {
                out,
                interactive,
                active: Vec::new(),
                drawn: 0,
                last_report: None,
            }),
        }
    }

    fn screen(&self) -> MutexGuard<'_, Screen> {
        self.screen
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl Default for ProgressPrinter {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    /// Print `message` on a line of its own, then put the progress line back.
    fn say(&mut self, message: &str) {
        let mut text = self.wipe();
        text.push_str(message);
        text.push('\n');
        text.push_str(&self.draw());
        self.write(&text);
    }

    /// The text that wipes the progress line from the screen, if one is there.
    fn wipe(&mut self) -> String {
        if self.drawn == 0 {
            return String::new();
        }
        let wiped = format!("\r{}\r", " ".repeat(self.drawn));
        self.drawn = 0;
        wiped
    }

    /// The text that draws the progress line for the copies in flight, if any.
    ///
    /// A shorter line is padded to the width of the one it replaces.
    fn draw(&mut self) -> String {
        if !self.interactive || self.active.is_empty() {
            return String::new();
        }
        let line = self.summary();
        let width = line.chars().count().max(self.drawn);
        self.drawn = width;
        format!("\r{line:<width$}")
    }

    fn summary(&self) -> String {
        let done: u64 = self.active.iter().map(|copy| copy.done).sum();
        let total: u64 = self.active.iter().map(|copy| copy.total).sum();
        let percent = (done * 100).checked_div(total).unwrap_or(100);
        let (done, total) = (format_bytes(done), format_bytes(total));
        match self.active.as_slice() {
            [only] => format!("  {}: {percent}% ({done} of {total})", only.key),
            many => format!("  {} bundles: {percent}% ({done} of {total})", many.len()),
        }
    }

    fn write(&mut self, text: &str) {
        if !text.is_empty() {
            let _ = self.out.write_all(text.as_bytes());
            let _ = self.out.flush();
        }
    }
}

impl Observer for ProgressPrinter {
    fn waiting(&self, key: &EntryKey) {
        self.screen().say(&format!(
            "envoy: waiting for another envoy to finish caching {key}..."
        ));
    }

    fn started(&self, key: &EntryKey, files: u64, bytes: u64) {
        let mut screen = self.screen();
        screen.active.push(Active {
            key: key.to_string(),
            done: 0,
            total: bytes,
        });
        screen.say(&format!(
            "envoy: caching {key} ({files} files, {}) on first use...",
            format_bytes(bytes)
        ));
    }

    fn progress(&self, key: &EntryKey, done: u64, total: u64) {
        let mut screen = self.screen();
        let key = key.to_string();
        if let Some(copy) = screen.active.iter_mut().find(|copy| copy.key == key) {
            copy.done = done;
            copy.total = total;
        }

        if screen.interactive {
            let text = screen.draw();
            screen.write(&text);
        } else if screen
            .last_report
            .is_none_or(|reported| reported.elapsed() >= LINE_PROGRESS_INTERVAL)
        {
            screen.last_report = Some(Instant::now());
            let text: String = screen
                .active
                .iter()
                .map(|copy| {
                    let percent = (copy.done * 100).checked_div(copy.total).unwrap_or(100);
                    format!("  {}: {percent}%\n", copy.key)
                })
                .collect();
            screen.write(&text);
        }
    }

    fn finished(&self, key: &EntryKey, _elapsed: Duration) {
        let mut screen = self.screen();
        let key = key.to_string();
        screen.active.retain(|copy| copy.key != key);
        let mut text = screen.wipe();
        text.push_str(&screen.draw());
        screen.write(&text);
    }

    fn cached(&self, key: &EntryKey, bytes: u64, elapsed: Duration) {
        self.screen().say(&format!(
            "envoy: cached {key} ({}) in {:.1} s",
            format_bytes(bytes),
            elapsed.as_secs_f64()
        ));
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
    use std::io;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn bytes_are_formatted_for_people() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(668_150_000), "637.2 MB");
    }

    /// Everything written, kept for the test to read.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture locks")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("capture locks").clone()).expect("text")
        }

        /// The lines a terminal would show: a carriage return sends the cursor
        /// back to the start of the line, and what is written next covers
        /// what was there.
        fn screen(&self) -> Vec<String> {
            let mut lines: Vec<Vec<char>> = vec![Vec::new()];
            let mut column = 0;
            for character in self.text().chars() {
                match character {
                    '\n' => {
                        lines.push(Vec::new());
                        column = 0;
                    }
                    '\r' => column = 0,
                    other => {
                        let line = lines.last_mut().expect("there is a line");
                        if column < line.len() {
                            line[column] = other;
                        } else {
                            line.push(other);
                        }
                        column += 1;
                    }
                }
            }
            lines
                .into_iter()
                .map(|line| line.into_iter().collect::<String>().trim_end().to_string())
                .collect()
        }
    }

    fn printer(interactive: bool) -> (ProgressPrinter, Capture) {
        let capture = Capture::default();
        let printer = ProgressPrinter::writing_to(Box::new(capture.clone()), interactive);
        (printer, capture)
    }

    fn key(name: &str) -> EntryKey {
        EntryKey::new("ext", name, "1.0.0").expect("test key is valid")
    }

    #[test]
    fn one_copy_shows_its_own_name_and_leaves_no_line_behind() {
        let (printer, capture) = printer(true);
        let python = key("python");

        printer.started(&python, 3, 2048);
        printer.progress(&python, 1024, 2048);
        assert_eq!(
            capture.screen(),
            [
                "envoy: caching ext:python@1.0.0 (3 files, 2.0 KB) on first use...",
                "  ext:python@1.0.0: 50% (1.0 KB of 2.0 KB)",
            ]
        );

        printer.finished(&python, Duration::from_secs(2));
        printer.cached(&python, 2048, Duration::from_millis(1500));
        assert_eq!(
            capture.screen(),
            [
                "envoy: caching ext:python@1.0.0 (3 files, 2.0 KB) on first use...",
                "envoy: cached ext:python@1.0.0 (2.0 KB) in 1.5 s",
                "",
            ],
            "the progress line is wiped when the last copy ends"
        );
    }

    #[test]
    fn copies_in_flight_share_one_line_and_no_message_lands_inside_it() {
        let (printer, capture) = printer(true);
        let (python, pyside) = (key("python"), key("pyside"));

        printer.started(&python, 10, 1000);
        printer.started(&pyside, 20, 3000);
        printer.progress(&python, 500, 1000);
        printer.progress(&pyside, 1500, 3000);
        assert_eq!(
            capture.screen().last().map(String::as_str),
            Some("  2 bundles: 50% (2.0 KB of 3.9 KB)")
        );

        printer.finished(&python, Duration::from_secs(1));
        printer.cached(&python, 1000, Duration::from_secs(1));
        assert_eq!(
            capture.screen(),
            [
                "envoy: caching ext:python@1.0.0 (10 files, 1000 B) on first use...",
                "envoy: caching ext:pyside@1.0.0 (20 files, 2.9 KB) on first use...",
                "envoy: cached ext:python@1.0.0 (1000 B) in 1.0 s",
                "  ext:pyside@1.0.0: 50% (1.5 KB of 2.9 KB)",
            ],
            "the message has a line of its own and the line for the copy still running is back"
        );
    }

    #[test]
    fn a_shorter_line_covers_the_longer_one_it_replaces() {
        let (printer, capture) = printer(true);
        let (python, pyside) = (key("python"), key("pyside"));

        printer.started(&python, 1, 1000);
        printer.started(&pyside, 1, 1000);
        printer.progress(&python, 100, 1000);
        printer.finished(&pyside, Duration::from_secs(1));

        let screen = capture.screen();
        assert_eq!(
            screen.last().map(String::as_str),
            Some("  ext:python@1.0.0: 10% (100 B of 1000 B)"),
            "nothing is left of '2 bundles: ...'"
        );
    }

    #[test]
    fn a_log_gets_a_line_per_copy_now_and_then_and_no_carriage_returns() {
        let (printer, capture) = printer(false);
        let (python, pyside) = (key("python"), key("pyside"));

        printer.started(&python, 10, 1000);
        printer.started(&pyside, 20, 3000);
        printer.progress(&python, 500, 1000);
        printer.progress(&python, 600, 1000);
        printer.finished(&python, Duration::from_secs(1));
        printer.cached(&python, 1000, Duration::from_secs(1));

        assert!(
            !capture.text().contains('\r'),
            "a log is never rewritten:\n{:?}",
            capture.text()
        );
        assert_eq!(
            capture.screen(),
            [
                "envoy: caching ext:python@1.0.0 (10 files, 1000 B) on first use...",
                "envoy: caching ext:pyside@1.0.0 (20 files, 2.9 KB) on first use...",
                "  ext:python@1.0.0: 50%",
                "  ext:pyside@1.0.0: 0%",
                "envoy: cached ext:python@1.0.0 (1000 B) in 1.0 s",
                "",
            ],
            "the second report came too soon after the first to be printed"
        );
    }
}
