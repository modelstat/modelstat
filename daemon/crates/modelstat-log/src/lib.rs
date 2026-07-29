//! One-line stderr logging for modelstat's long-running processes.
//!
//! The daemon, the summariser engine and the MCP bridge are supervised (launchd
//! / systemd / Task Scheduler) and their stderr is appended to a log file that a
//! human reads days later, usually while debugging. Bare `eprintln!` carries no
//! time, so in a report like
//!
//! ```text
//! [modelstat] embedder: BGE model not loadable at …
//! [modelstat] NER (redaction layer 2): candle BERT-NER loaded
//! [modelstat] embedder: BGE model not loadable at …
//! ```
//!
//! there is no way to tell one process retrying from a service crash-looping —
//! the two have completely different fixes. That is the whole reason this crate
//! exists.
//!
//! # The two renderings
//!
//! The mode is decided **once per process**, at startup:
//!
//! | mode                              | line                                          |
//! |-----------------------------------|-----------------------------------------------|
//! | service (after [`init_service`])  | `2026-07-29T09:14:22.481Z WARN  embedder: …`  |
//! | interactive (the default)         | `modelstat: embedder: …`                      |
//!
//! It is an explicit call, never a TTY probe: `modelstat start` run by hand in a
//! terminal *is* the daemon, and must produce byte-identical logs to the
//! supervised one — a line that renders two different ways depending on where
//! stderr points is a line you cannot ask a user to paste back to you.
//!
//! # Everything goes to stderr
//!
//! Including what used to be `println!`. stdout is a program's *output*; a
//! daemon's diagnostics are not output, and splitting them over `out.log` and
//! `err.log` only ever hid half the story from whoever was reading one of them.
//!
//! # Not a log framework
//!
//! No levels-as-filters, no targets, no subscribers. Three severities that a
//! human skims for, and that is the entire surface. Reach for `tracing` the day
//! these logs need to be machine-parsed or shipped somewhere — not before.

use std::fmt::Arguments;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether this process renders the service (timestamped) form. Set once at
/// startup by [`init_service`] and only ever read afterwards, so `Relaxed` is
/// the right ordering — there is nothing else being published alongside it.
static SERVICE: AtomicBool = AtomicBool::new(false);

/// How loud a line is. Purely a reading aid — nothing filters on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Something happened that a reader would want confirmed. The default.
    Info,
    /// Degraded but still running. Someone should look, eventually.
    Warn,
    /// A thing that was supposed to work did not.
    Error,
}

impl Level {
    /// The fixed-width column label. Padded to 5 so the message column of a log
    /// file lines up and stays scannable.
    pub fn label(self) -> &'static str {
        match self {
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }
}

/// Switch this process to the service (timestamped) rendering. Call it once,
/// first thing, in every entrypoint that runs supervised and forever: the
/// daemon, the summariser engine, the MCP bridge, the foreground watcher.
///
/// Idempotent, and safe to call from anywhere — but calling it late means the
/// lines logged before it are already gone, untimestamped.
pub fn init_service() {
    SERVICE.store(true, Ordering::Relaxed);
}

/// Whether [`init_service`] has run. Exposed for the few call sites that render
/// their own bytes (the download progress bar's terminal redraw) and need to
/// know which shape to produce.
pub fn is_service() -> bool {
    SERVICE.load(Ordering::Relaxed)
}

/// Render one message into its final log text (no trailing newline).
///
/// Every line of a multi-line message gets its own prefix. A log file where some
/// lines start with a timestamp and others do not cannot be sorted, grepped by
/// time, or read top-down — so the invariant here is total: **one output line,
/// one prefix**, whatever the caller passed.
///
/// `now` is threaded in rather than read from the clock so the formatting is
/// testable against a fixed instant. It is ignored in interactive mode.
pub fn render(service: bool, level: Level, now: &str, msg: &str) -> String {
    let prefix = if service {
        format!("{now} {} ", level.label())
    } else {
        "modelstat: ".to_string()
    };
    let mut out = String::with_capacity(msg.len() + prefix.len() + 16);
    for (i, line) in msg.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&prefix);
        out.push_str(line);
    }
    out
}

/// The macros' entrypoint. Not called directly.
#[doc(hidden)]
pub fn emit(level: Level, args: Arguments<'_>) {
    let service = is_service();
    // Only pay for the clock when the timestamp is actually rendered.
    let now = if service {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    } else {
        String::new()
    };
    let mut line = render(service, level, &now, &args.to_string());
    line.push('\n');
    // One `write_all` under one lock. The daemon logs from many threads at once,
    // and two half-written lines spliced together are worse than either line
    // missing — the reader can't tell it happened.
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(line.as_bytes());
    let _ = err.flush();
}

/// Log at [`Level::Info`]. Same formatting args as `println!`.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::emit($crate::Level::Info, format_args!($($arg)*)) };
}

/// Log at [`Level::Warn`] — degraded, still running.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::emit($crate::Level::Warn, format_args!($($arg)*)) };
}

/// Log at [`Level::Error`] — something that should have worked did not.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::emit($crate::Level::Error, format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "2026-07-29T09:14:22.481Z";

    #[test]
    fn service_lines_lead_with_the_timestamp_and_a_padded_level() {
        assert_eq!(
            render(true, Level::Warn, T, "embedder: BGE model not loadable"),
            "2026-07-29T09:14:22.481Z WARN  embedder: BGE model not loadable"
        );
        assert_eq!(
            render(true, Level::Error, T, "ingest fetch failed"),
            "2026-07-29T09:14:22.481Z ERROR ingest fetch failed"
        );
        // INFO is padded to the same width as ERROR so the message column aligns.
        let info = render(true, Level::Info, T, "x");
        let error = render(true, Level::Error, T, "x");
        assert_eq!(info.find('x'), error.find('x'));
    }

    #[test]
    fn interactive_lines_carry_no_clock_and_read_as_cli_output() {
        for level in [Level::Info, Level::Warn, Level::Error] {
            assert_eq!(
                render(false, level, T, "daemon healthy"),
                "modelstat: daemon healthy"
            );
        }
    }

    /// The invariant that makes the log file greppable by time.
    #[test]
    fn every_line_of_a_multi_line_message_is_prefixed() {
        let out = render(true, Level::Info, T, "first\nsecond\nthird");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 3);
        for line in lines {
            assert!(line.starts_with(T), "unprefixed line: {line:?}");
        }
        // …and the same in interactive mode.
        let out = render(false, Level::Info, T, "first\nsecond");
        assert!(out.split('\n').all(|l| l.starts_with("modelstat: ")));
    }

    /// An empty message still produces exactly one prefixed line, never a bare
    /// blank one that would break a time-sorted read.
    #[test]
    fn empty_message_is_still_one_prefixed_line() {
        assert_eq!(render(true, Level::Info, T, ""), format!("{T} INFO  "));
        assert_eq!(render(false, Level::Info, T, ""), "modelstat: ");
    }

    /// A trailing newline in the message must not smuggle in an unprefixed line.
    #[test]
    fn trailing_newline_does_not_produce_an_unprefixed_line() {
        let out = render(true, Level::Info, T, "done\n");
        assert!(out.split('\n').all(|l| l.starts_with(T)), "{out:?}");
    }

    /// Interactive is the DEFAULT, and the switch is one-way. A regression here
    /// would stamp a timestamp on every line of `modelstat status`.
    #[test]
    fn mode_defaults_to_interactive_and_init_service_latches_it() {
        assert!(!is_service(), "a process must start out interactive");
        init_service();
        assert!(is_service());
        init_service(); // idempotent
        assert!(is_service());
        // Both macros still work after the flip (they write to the real stderr;
        // this asserts they compile + don't panic, the shape is `render`'s job).
        log_info!("emitted from {}", "a test");
        log_warn!("warn");
        log_error!("error");
    }
}
