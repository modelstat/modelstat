//! Download progress rendering (feature §11): a single redrawing TTY line every
//! ~200ms, or a full line every ~2s on a non-TTY, plus a completion line.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

/// Where download progress is reported. Throttling (the 200ms / 2s cadence) is
/// applied by the downloader; a sink just renders what it's handed.
pub trait ProgressSink: Send + Sync {
    /// True for a terminal (drives the redraw cadence + `\r` vs newline).
    fn is_tty(&self) -> bool {
        false
    }
    /// Called once before bytes flow, with the known size label (if any).
    fn start(&self, _label: &str, _size_label: Option<&str>, _total: Option<u64>) {}
    /// Called periodically with cumulative bytes + elapsed time.
    fn progress(&self, _downloaded: u64, _total: Option<u64>, _elapsed: Duration) {}
    /// Called once on success with the final path.
    fn done(&self, _path: &Path) {}
}

/// A no-op sink (tests, non-interactive callers that don't want output).
pub struct SilentSink;
impl ProgressSink for SilentSink {}

/// Renders to stderr — a redrawing line on a TTY, periodic full lines otherwise.
pub struct TtyProgress {
    tty: bool,
    label: String,
}

impl TtyProgress {
    /// Build a reporter for `label` (the artifact name in the progress line),
    /// auto-detecting whether stderr is a terminal.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            tty: std::io::stderr().is_terminal(),
            label: label.into(),
        }
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1_000_000.0
}

impl ProgressSink for TtyProgress {
    fn is_tty(&self) -> bool {
        self.tty
    }

    fn start(&self, _label: &str, size_label: Option<&str>, _total: Option<u64>) {
        let size = size_label.map(|s| format!(" ({s})")).unwrap_or_default();
        modelstat_log::log_info!("downloading {}{size}…", self.label);
    }

    fn progress(&self, downloaded: u64, total: Option<u64>, elapsed: Duration) {
        let secs = elapsed.as_secs_f64().max(0.001);
        let rate = mb(downloaded) / secs; // MB/s
        let line = match total {
            Some(t) if t > 0 => {
                let pct = (downloaded as f64 / t as f64 * 100.0).min(100.0);
                let remaining = t.saturating_sub(downloaded);
                let eta = if rate > 0.0 {
                    (mb(remaining) / rate) as u64
                } else {
                    0
                };
                format!(
                    "{:.0} / {:.0} MB ({:.0}%) · {:.1} MB/s · ETA {}s · {:.0}s",
                    mb(downloaded),
                    mb(t),
                    pct,
                    rate,
                    eta,
                    secs,
                )
            }
            _ => format!("{:.0} MB · {:.1} MB/s · {:.0}s", mb(downloaded), rate, secs),
        };
        // The TTY path is a redraw, not a log line — it rewrites one line in place
        // and must stay raw bytes (a timestamp on a line that's about to be
        // overwritten 5× a second is noise). Off a TTY it's a real log line.
        if self.tty {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r  {line}\x1b[K");
            let _ = err.flush();
        } else {
            modelstat_log::log_info!("  {line}");
        }
    }

    fn done(&self, path: &Path) {
        if self.tty {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r\x1b[K"); // clear the redraw line
            let _ = err.flush();
        }
        modelstat_log::log_info!("✓ {} → {}", self.label, path.display());
    }
}
