//! `modelstat-download` — the shared model-artifact downloader (feature §11).
//!
//! One resume-safe downloader for every model file: the engine's Qwen GGUF
//! (~2.7 GB) and the collector's PII detector (~900 MB) + embedder (~130 MB) models.
//! Resume-safe (`.partial` + `Range` + atomic rename), sha256-verified when a
//! digest is pinned, with a throttled progress meter (single redrawing TTY line
//! / a periodic non-TTY line).
//!
//! # Retry
//!
//! [`download`] makes exactly one attempt. [`download_with_retry`] — and the
//! `RetryPolicy` every [`hf`] entry point takes — re-attempts on anything
//! transient (dropped connection, timeout, 408/429/5xx) with exponential backoff,
//! and gives up immediately on anything permanent (a 404 URL, a checksum
//! mismatch), since retrying those forever would only hide a real bug.
//!
//! Because every attempt resumes from the `.partial` via `Range`, a retry
//! continues from where the last one stopped instead of restarting the transfer.
//! That is what makes an unbounded [`RetryPolicy::forever`] safe for the daemon's
//! background self-heal: a machine offline for a day resumes mid-file when it
//! comes back.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

pub mod hf;
mod progress;
pub use hf::{download_hf_model, ensure_hf_model, HfModel, BGE_SMALL, PRIVACY_FILTER};

pub use progress::{ProgressSink, SilentSink, TtyProgress};

/// What to download and where.
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub url: String,
    /// Final destination. The download streams to `<dest>.partial` and renames.
    pub dest: PathBuf,
    /// Pinned sha256 (lowercase hex). Verified after download; a mismatch is a
    /// hard error and the `.partial` is deleted. None = no verification.
    pub expected_sha256: Option<String>,
    /// Human label for the known size, printed before the download starts
    /// (e.g. `"~2.7 GB"`).
    pub size_label: Option<String>,
    /// Short artifact name for the progress line (e.g. `"Qwen3.5-4B"`).
    pub label: String,
}

/// A download failure. Best-effort by contract — the caller treats any error as
/// "not yet available" and retries later; it never aborts an install.
#[derive(Debug)]
pub enum DownloadError {
    Http(u16),
    Transport(String),
    Io(std::io::Error),
    /// The finished file's sha256 didn't match the pinned digest.
    ChecksumMismatch {
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::Http(c) => write!(f, "download failed: HTTP {c}"),
            DownloadError::Transport(m) => write!(f, "download failed: {m}"),
            DownloadError::Io(e) => write!(f, "download failed: {e}"),
            DownloadError::ChecksumMismatch { expected, actual } => write!(
                f,
                "download checksum mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for DownloadError {}

impl From<std::io::Error> for DownloadError {
    fn from(e: std::io::Error) -> Self {
        DownloadError::Io(e)
    }
}

impl DownloadError {
    /// Whether re-attempting could plausibly succeed.
    ///
    /// Transient: transport faults (reset, DNS, timeout), 408/429, and every 5xx
    /// — the network or the far side, both of which come back. Local I/O counts
    /// too: a full disk or a locked file clears once someone frees it, and the
    /// alternative is a daemon that gives up permanently on a temporary state.
    ///
    /// Permanent: any other 4xx (a 404 means the URL is wrong — retrying it for a
    /// week just hides the bug) and a checksum mismatch (the bytes are wrong, and
    /// a pinned digest doesn't fix itself).
    pub fn is_transient(&self) -> bool {
        match self {
            DownloadError::Transport(_) | DownloadError::Io(_) => true,
            DownloadError::Http(code) => *code == 408 || *code == 429 || *code >= 500,
            DownloadError::ChecksumMismatch { .. } => false,
        }
    }
}

/// How hard to try. Backoff doubles from `initial` up to `max`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// `None` = keep going until it succeeds or fails permanently.
    pub max_attempts: Option<u32>,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl RetryPolicy {
    /// For a human waiting at a terminal: ~4 tries over ~30s, then report and let
    /// them get on with it. Never leave someone staring at a frozen `connect`.
    pub fn interactive() -> Self {
        Self {
            max_attempts: Some(4),
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(16),
        }
    }

    /// For a background task with nobody waiting: never stop. Backoff settles at
    /// 5 minutes, so an offline machine costs one cheap failed connect per 5 min
    /// and resumes the moment the network returns.
    pub fn forever() -> Self {
        Self {
            max_attempts: None,
            initial_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(5 * 60),
        }
    }

    /// The delay before attempt `attempt` (0-based; `backoff(0)` precedes the
    /// second attempt). Doubles, clamped to `max_backoff`.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u64.checked_shl(attempt.min(32)).unwrap_or(u64::MAX);
        self.initial_backoff
            .saturating_mul(factor.min(u32::MAX as u64) as u32)
            .min(self.max_backoff)
    }

    /// Whether another attempt is allowed after `attempts_made` have failed.
    pub fn should_retry(&self, attempts_made: u32) -> bool {
        match self.max_attempts {
            None => true,
            Some(max) => attempts_made < max,
        }
    }
}

/// [`download`], re-attempted per `policy` while the failure is transient.
///
/// Every attempt resumes from the `.partial`, so a retry continues the transfer
/// rather than restarting it. Each failure is logged loudly with the attempt
/// number and the wait before the next one — a stalled download must be visible
/// in the log, never a silent gap.
pub async fn download_with_retry(
    client: &reqwest::Client,
    spec: &DownloadSpec,
    sink: &dyn ProgressSink,
    policy: &RetryPolicy,
) -> Result<PathBuf, DownloadError> {
    let mut attempts = 0u32;
    loop {
        match download(client, spec, sink).await {
            Ok(path) => return Ok(path),
            Err(e) => {
                attempts += 1;
                if !e.is_transient() {
                    modelstat_log::log_error!(
                        "download of {} failed permanently: {e} — this will not resolve on its own",
                        spec.label
                    );
                    return Err(e);
                }
                if !policy.should_retry(attempts) {
                    modelstat_log::log_error!(
                        "download of {} gave up after {attempts} attempts: {e}",
                        spec.label
                    );
                    return Err(e);
                }
                let wait = policy.backoff(attempts - 1);
                modelstat_log::log_warn!(
                    "download of {} failed (attempt {attempts}): {e} — retrying in {}s, resuming where it stopped",
                    spec.label,
                    wait.as_secs()
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// Download `spec` to its destination, reporting progress to `sink`. Resumes a
/// prior `.partial` when the server supports `Range`; verifies sha256 when
/// pinned; renames atomically on success. Returns the final path.
///
/// ONE attempt — see [`download_with_retry`] when the download has to land.
pub async fn download(
    client: &reqwest::Client,
    spec: &DownloadSpec,
    sink: &dyn ProgressSink,
) -> Result<PathBuf, DownloadError> {
    if spec.dest.exists() {
        return Ok(spec.dest.clone()); // already present (self-healing idempotency)
    }
    if let Some(parent) = spec.dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let partial = partial_path(&spec.dest);

    // Resume from any prior `.partial`.
    let mut existing = tokio::fs::metadata(&partial)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut req = client.get(&spec.url);
    if existing > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={existing}-"));
    }

    let resp = req.send().await.map_err(map_transport)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(DownloadError::Http(status.as_u16()));
    }
    // A 200 (not 206) to a Range request means no resume support → restart.
    let resuming = status.as_u16() == 206 && existing > 0;
    if !resuming {
        existing = 0;
        let _ = tokio::fs::remove_file(&partial).await;
    }

    // Total = already-on-disk + remaining (content-length is the REMAINING bytes
    // on a 206). None when the server doesn't report a length.
    let total = resp.content_length().map(|len| existing + len);
    sink.start(&spec.label, spec.size_label.as_deref(), total);

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&partial)
        .await?;
    let mut downloaded = existing;
    let started = Instant::now();
    let mut last_report = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .unwrap_or_else(Instant::now);
    let report_every = if sink.is_tty() {
        Duration::from_millis(200)
    } else {
        Duration::from_secs(2)
    };

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_transport)?;
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        if last_report.elapsed() >= report_every {
            sink.progress(downloaded, total, started.elapsed());
            last_report = Instant::now();
        }
    }
    file.flush().await?;
    drop(file);
    sink.progress(downloaded, total, started.elapsed());

    if let Some(expected) = &spec.expected_sha256 {
        let actual = sha256_file(&partial).await?;
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(DownloadError::ChecksumMismatch {
                expected: expected.clone(),
                actual,
            });
        }
    }

    tokio::fs::rename(&partial, &spec.dest).await?;
    sink.done(&spec.dest);
    Ok(spec.dest.clone())
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".partial");
    PathBuf::from(s)
}

fn map_transport(e: reqwest::Error) -> DownloadError {
    DownloadError::Transport(e.to_string())
}

/// sha256 of a file, streamed so a multi-GB artifact never fully materialises.
async fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex)
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    /// The classification that decides whether the daemon keeps trying for days
    /// or gives up and tells a human. Getting either side wrong is bad: a
    /// transient marked permanent strands the model forever (the bug this whole
    /// change exists to kill); a permanent marked transient hides a broken URL
    /// behind an infinite quiet loop.
    #[test]
    fn transient_covers_what_comes_back_and_nothing_else() {
        assert!(DownloadError::Transport("connection reset".into()).is_transient());
        assert!(DownloadError::Io(std::io::Error::other("disk full")).is_transient());
        for code in [408, 429, 500, 502, 503, 504, 599] {
            assert!(
                DownloadError::Http(code).is_transient(),
                "HTTP {code} should retry"
            );
        }
        for code in [400, 401, 403, 404, 410, 451] {
            assert!(
                !DownloadError::Http(code).is_transient(),
                "HTTP {code} must NOT retry — it will never succeed"
            );
        }
        assert!(!DownloadError::ChecksumMismatch {
            expected: "a".into(),
            actual: "b".into(),
        }
        .is_transient());
    }

    #[test]
    fn backoff_doubles_then_clamps() {
        let p = RetryPolicy {
            max_attempts: None,
            initial_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(60),
        };
        assert_eq!(p.backoff(0), Duration::from_secs(5));
        assert_eq!(p.backoff(1), Duration::from_secs(10));
        assert_eq!(p.backoff(2), Duration::from_secs(20));
        assert_eq!(p.backoff(3), Duration::from_secs(40));
        assert_eq!(p.backoff(4), Duration::from_secs(60)); // clamped
                                                           // A far-future attempt must clamp, never overflow into a tiny/huge wait.
        assert_eq!(p.backoff(4_000_000_000), Duration::from_secs(60));
    }

    #[test]
    fn forever_never_stops_and_interactive_does() {
        let f = RetryPolicy::forever();
        assert!(f.should_retry(0));
        assert!(f.should_retry(10_000));
        assert!(f.backoff(99) <= Duration::from_secs(5 * 60));

        let i = RetryPolicy::interactive();
        assert!(i.should_retry(1));
        assert!(!i.should_retry(4), "a human must not wait past the budget");
        // The whole interactive budget stays well under a minute of waiting.
        let total: Duration = (0..4).map(|a| i.backoff(a)).sum();
        assert!(
            total < Duration::from_secs(60),
            "budget too slow: {total:?}"
        );
    }
}
