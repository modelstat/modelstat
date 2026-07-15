//! `modelstat-download` — the shared model-artifact downloader (feature §11).
//!
//! One resume-safe downloader for every model file: the engine's Qwen GGUF
//! (~2.7 GB) and the collector's NER (~250 MB) + embedder (~50–130 MB) models.
//! Resume-safe (`.partial` + `Range` + atomic rename), sha256-verified when a
//! digest is pinned, with a throttled progress meter (single redrawing TTY line
//! / a periodic non-TTY line). Download failures never fail an install — the
//! caller lazy-downloads on first use and self-heals (§9.4/§9.5).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

mod progress;
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
    ChecksumMismatch { expected: String, actual: String },
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

/// Download `spec` to its destination, reporting progress to `sink`. Resumes a
/// prior `.partial` when the server supports `Range`; verifies sha256 when
/// pinned; renames atomically on success. Returns the final path.
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
    let mut existing = tokio::fs::metadata(&partial).await.map(|m| m.len()).unwrap_or(0);
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
