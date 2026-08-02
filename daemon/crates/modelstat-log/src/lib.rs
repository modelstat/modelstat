//! One-line logging for modelstat's long-running processes.
//!
//! The daemon, the summariser engine and the MCP bridge are supervised (launchd
//! / systemd / Task Scheduler) and their streams are appended to log files that a
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
//! The [`Mode`] is decided **once per process**, at startup:
//!
//! | mode                        | line                                          |
//! |-----------------------------|-----------------------------------------------|
//! | either supervised mode      | `2026-07-29T09:14:22.481Z WARN  embedder: …`  |
//! | interactive (the default)   | `modelstat: embedder: …`                      |
//!
//! It is an explicit call, never a TTY probe: `modelstat start` run by hand in a
//! terminal *is* the daemon, and must produce byte-identical logs to the
//! supervised one — a line that renders two different ways depending on where
//! its stream points is a line you cannot ask a user to paste back to you.
//!
//! # Which stream a line goes to
//!
//! Severity picks the stream, so that the supervisor's own `out.log`/`err.log`
//! split carries meaning: launchd's `StandardOutPath` and systemd's
//! `StandardOutput=` collect the routine narration, `StandardErrorPath` /
//! `StandardError=` collect only what is actually wrong.
//!
//! ```text
//! INFO   → stdout → out.log
//! WARN   → stderr → err.log
//! ERROR  → stderr → err.log
//! ```
//!
//! Reading one incident therefore means merging two files by timestamp — which
//! is why every service line leads with one, and why the daemon must never write
//! anything to either stream except through this crate.
//!
//! # stdout is not always free
//!
//! Some processes already use stdout as a *data* channel: the MCP bridge writes
//! JSON-RPC frames on it, and `_daemon-health` prints JSON the tray parses. A
//! single INFO line on those streams is a protocol error, not a log line. Those
//! entrypoints call [`init_service_stdout_reserved`] instead, and every level
//! goes to stderr for them. It is per-entrypoint and explicit — never inferred —
//! so no process has to guess what its own stdout is for.
//!
//! # Not a log framework
//!
//! No levels-as-filters, no targets, no subscribers. Three severities that a
//! human skims for, and that is the entire surface. Reach for `tracing` the day
//! these logs need to be machine-parsed or shipped somewhere — not before.

use std::fmt::Arguments;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

/// How this process renders and routes its lines. Set once at startup, then only
/// ever read, so `Relaxed` is the right ordering — there is nothing else being
/// published alongside it.
static MODE: AtomicU8 = AtomicU8::new(Mode::Interactive as u8);

/// How this process renders and routes its lines. Picked once at startup by the
/// entrypoint, which is the only code that knows whether its own stdout is free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// A human is watching the command run. `modelstat: …`, no clock, every
    /// level on stderr so it never lands in the middle of a command's real
    /// output. The default, and what every interactive verb keeps.
    Interactive = 0,
    /// Supervised and long-running, with stdout free: timestamped, and the
    /// severity picks the stream (INFO → stdout, WARN/ERROR → stderr) so the
    /// supervisor files them into `out.log` / `err.log`.
    Service = 1,
    /// Supervised, but stdout already carries data a machine parses — JSON-RPC
    /// frames, a JSON document. Timestamped like [`Mode::Service`], but every
    /// level goes to stderr: on these processes an INFO line on stdout is a
    /// protocol error, not a log line.
    ServiceStdoutReserved = 2,
}

impl Mode {
    /// Whether lines in this mode lead with a timestamp. Both supervised modes
    /// do; only the interactive one does not.
    pub fn is_timestamped(self) -> bool {
        self != Mode::Interactive
    }
}

/// How loud a line is. A reading aid, and — in [`Mode::Service`] — what picks
/// the stream the line is written to. See [`goes_to_stdout`].
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

/// Switch this process to [`Mode::Service`]: timestamped, INFO on stdout and
/// WARN/ERROR on stderr. Call it once, first thing, in every entrypoint that
/// runs supervised and forever *and* leaves stdout unused — the daemon, the
/// summariser engine, the foreground watcher.
///
/// "Free" means nothing *parses* this process's stdout. Human-readable
/// decoration is fine to interleave with (`_install-service`'s "✓ installed"
/// banner, which only ever gets skimmed in a log); a JSON document or a protocol
/// frame is not, and those entrypoints want [`init_service_stdout_reserved`].
///
/// Calling it late means the lines logged before it are already gone,
/// untimestamped — so it goes first, before any fallible setup.
pub fn init_service() {
    MODE.store(Mode::Service as u8, Ordering::Relaxed);
}

/// Switch this process to [`Mode::ServiceStdoutReserved`]: timestamped like
/// [`init_service`], but every level on stderr because stdout is carrying data
/// somebody parses. Call it once, first thing, from the MCP stdio bridge and the
/// JSON-emitting machine verbs.
pub fn init_service_stdout_reserved() {
    MODE.store(Mode::ServiceStdoutReserved as u8, Ordering::Relaxed);
}

/// This process's mode. Defaults to [`Mode::Interactive`] until an entrypoint
/// says otherwise.
pub fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        x if x == Mode::Service as u8 => Mode::Service,
        x if x == Mode::ServiceStdoutReserved as u8 => Mode::ServiceStdoutReserved,
        _ => Mode::Interactive,
    }
}

/// The whole routing table: a line goes to stdout only when it is routine
/// narration from a process whose stdout is free. Everything else — every
/// warning, every error, and every line from a process holding stdout open for
/// data — goes to stderr.
///
/// Pure, so the table is asserted directly rather than by capturing real file
/// descriptors.
pub fn goes_to_stdout(mode: Mode, level: Level) -> bool {
    mode == Mode::Service && level == Level::Info
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
    let mode = mode();
    // Only pay for the clock when the timestamp is actually rendered.
    let now = if mode.is_timestamped() {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    } else {
        String::new()
    };
    let mut line = render(mode.is_timestamped(), level, &now, &args.to_string());
    line.push('\n');
    let bytes = line.as_bytes();
    // One `write_all` under one lock, on whichever stream this line belongs to.
    // The daemon logs from many threads at once, and two half-written lines
    // spliced together are worse than either line missing — the reader can't
    // tell it happened. Flushed every line: stdout is block-buffered once it is
    // a file rather than a terminal, and a crash must not take the last minute
    // of `out.log` with it.
    if goes_to_stdout(mode, level) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    } else {
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(bytes);
        let _ = err.flush();
    }
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

    /// The whole routing table, asserted directly. The two rules that matter:
    /// a warning must never be filed as routine narration, and a process whose
    /// stdout carries data must never have a log line land on it.
    #[test]
    fn only_info_from_a_stdout_free_service_goes_to_stdout() {
        assert!(goes_to_stdout(Mode::Service, Level::Info));
        assert!(!goes_to_stdout(Mode::Service, Level::Warn));
        assert!(!goes_to_stdout(Mode::Service, Level::Error));
        // stdout is holding JSON-RPC frames / a parsed JSON document: one INFO
        // line on it is a protocol error, so every level stays on stderr.
        for level in [Level::Info, Level::Warn, Level::Error] {
            assert!(
                !goes_to_stdout(Mode::ServiceStdoutReserved, level),
                "{level:?} leaked onto a reserved stdout"
            );
        }
        // Interactive keeps everything on stderr so a log line never lands in
        // the middle of a command's real output (`status --json`, `paths`).
        for level in [Level::Info, Level::Warn, Level::Error] {
            assert!(!goes_to_stdout(Mode::Interactive, level));
        }
    }

    /// Only the interactive mode is untimestamped; both supervised modes render
    /// identically, differing solely in where the bytes go.
    #[test]
    fn both_supervised_modes_are_timestamped_and_render_alike() {
        assert!(!Mode::Interactive.is_timestamped());
        assert!(Mode::Service.is_timestamped());
        assert!(Mode::ServiceStdoutReserved.is_timestamped());
        assert_eq!(
            render(Mode::Service.is_timestamped(), Level::Info, T, "x"),
            render(Mode::ServiceStdoutReserved.is_timestamped(), Level::Info, T, "x")
        );
    }

    /// Interactive is the DEFAULT. A regression here would stamp a timestamp on
    /// every line of `modelstat status` — and, worse, start routing INFO onto
    /// the stdout that `--json` verbs are writing their document to.
    ///
    /// The one test that touches the process-wide mode, so it owns every
    /// assertion about it (the others use the pure functions above).
    #[test]
    fn mode_defaults_to_interactive_and_the_entrypoint_sets_it() {
        assert_eq!(mode(), Mode::Interactive, "a process starts interactive");
        init_service();
        assert_eq!(mode(), Mode::Service);
        // All three macros work after the flip: this asserts they compile and
        // don't panic. The shape is `render`'s job, the stream is
        // `goes_to_stdout`'s, and that the real file descriptors agree is
        // `tests/stream_split.rs`'s.
        log_info!("emitted from {}", "a test");
        log_warn!("warn");
        log_error!("error");

        init_service_stdout_reserved();
        assert_eq!(mode(), Mode::ServiceStdoutReserved);
        log_info!("info with stdout reserved");
    }
}
