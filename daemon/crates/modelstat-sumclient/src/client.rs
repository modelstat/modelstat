//! Protocol-v1 client (feature §9 "Protocol client behavior"). Identical for
//! local (loopback) and self-hosted (org URL) modes.
//!
//! Retry matrix: per-request timeout `MODELSTAT_SUMMARIZER_TIMEOUT_MS` (default
//! 60s); ≤3 attempts on transport errors / 429 / 5xx (incl. the 503 the engine
//! returns while its model loads) with backoff `500ms·2^n` capped 8s, floor
//! 250ms, `Retry-After` honored clamped [250ms, 8s]; other 4xx fail immediately.
//! Error messages carry the STATUS only — never response bodies (§21.13).
//!
//! This is the low-level protocol client. The resilient hold-and-retry /
//! self-heal wrapper (60s cooldown, never a degraded fallback — §9.4) lives one
//! layer up in the pipeline; this layer just implements the ≤3-attempt protocol.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::header::HeaderMap;

use crate::protocol::{CompleteRequest, HealthResponse, PROTOCOL_VERSION};

const MAX_ATTEMPTS: u32 = 3;
const BACKOFF_BASE_MS: u64 = 500;
const BACKOFF_FLOOR_MS: u64 = 250;
const BACKOFF_CAP_MS: u64 = 8_000;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// A summarizer request failure. Deliberately carries the status only — never a
/// response body — so a secret-bearing echo can never leak through an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SumError {
    /// A transport/connection failure (no HTTP response). The text is the
    /// transport error's own message — reqwest transport errors never contain a
    /// response body.
    Transport(String),
    /// The request timed out.
    Timeout,
    /// The engine returned a non-2xx status (status only).
    Http(u16),
    /// The success body couldn't be parsed as protocol v1.
    Decode(String),
}

impl std::fmt::Display for SumError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SumError::Transport(m) => write!(f, "summarizer request failed: {m}"),
            SumError::Timeout => write!(f, "summarizer request timed out"),
            SumError::Http(code) => write!(f, "summarizer returned HTTP {code}"),
            SumError::Decode(m) => write!(f, "summarizer response was not protocol v1: {m}"),
        }
    }
}

impl std::error::Error for SumError {}

/// The summarizer engine client — one per resolved engine URL.
pub struct SummarizerClient {
    base_url: String,
    http: reqwest::Client,
    timeout: Duration,
    skew_warned: AtomicBool,
}

impl SummarizerClient {
    /// Build a client for `base_url` (e.g. `http://127.0.0.1:4321`). The
    /// per-request timeout comes from `MODELSTAT_SUMMARIZER_TIMEOUT_MS`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_timeout(base_url, resolve_timeout())
    }

    pub fn with_timeout(base_url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            timeout,
            skew_warned: AtomicBool::new(false),
        }
    }

    /// `GET /healthz` — a single-attempt liveness probe (fast up/down for
    /// preflight + status; the resilient wrapper retries the whole cycle).
    pub async fn healthz(&self) -> Result<HealthResponse, SumError> {
        let url = format!("{}/healthz", self.base_url);
        let resp = self
            .http
            .get(&url)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(map_transport)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SumError::Http(status.as_u16()));
        }
        resp.json::<HealthResponse>()
            .await
            .map_err(|e| SumError::Decode(e.to_string()))
    }

    /// `POST /v1/complete` — returns the completion text (reasoning already
    /// stripped by the engine). Applies the full §9 retry matrix.
    pub async fn complete(&self, req: &CompleteRequest) -> Result<String, SumError> {
        let url = format!("{}/v1/complete", self.base_url);
        let mut attempt = 0u32;
        loop {
            let send = self
                .http
                .post(&url)
                .timeout(self.timeout)
                .json(req)
                .send()
                .await;
            match send {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp
                            .json::<crate::protocol::CompleteResponse>()
                            .await
                            .map(|b| b.text)
                            .map_err(|e| SumError::Decode(e.to_string()));
                    }
                    let code = status.as_u16();
                    if is_retryable(code) && attempt + 1 < MAX_ATTEMPTS {
                        let retry_after = parse_retry_after(resp.headers());
                        drop(resp); // never read the body — status only
                        tokio::time::sleep(backoff(attempt, retry_after)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(SumError::Http(code));
                }
                Err(e) => {
                    if attempt + 1 < MAX_ATTEMPTS {
                        tokio::time::sleep(backoff(attempt, None)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(map_transport(e));
                }
            }
        }
    }

    /// Detect protocol/version skew against this collector. Returns the skew
    /// message for `status` (always, while skewed) and logs it to stderr exactly
    /// ONCE (feature §10.4). `own_version` is the collector's `daemon-<semver>`.
    pub fn check_skew(&self, health: &HealthResponse, own_version: &str) -> Option<String> {
        if health.protocol == PROTOCOL_VERSION && health.version == own_version {
            return None;
        }
        let msg = format!(
            "summarizer protocol/version skew: engine protocol {} v{}, collector protocol {} v{}",
            health.protocol, health.version, PROTOCOL_VERSION, own_version
        );
        if !self.skew_warned.swap(true, Ordering::Relaxed) {
            eprintln!("modelstat: ⚠ {msg}");
        }
        Some(msg)
    }
}

fn resolve_timeout() -> Duration {
    let ms = std::env::var("MODELSTAT_SUMMARIZER_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    Duration::from_millis(ms)
}

fn map_transport(e: reqwest::Error) -> SumError {
    if e.is_timeout() {
        SumError::Timeout
    } else {
        // reqwest transport errors carry the request URL + a kind, never a
        // response body; still, print the terse Display (no body exists to leak).
        SumError::Transport(e.to_string())
    }
}

/// 429 or any 5xx (incl. the 503 emitted while the model loads).
fn is_retryable(code: u16) -> bool {
    code == 429 || (500..=599).contains(&code)
}

/// The delay before the next attempt: `Retry-After` (clamped) when present, else
/// `500ms·2^attempt` clamped to [250ms, 8s].
fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(ra) = retry_after {
        return clamp_delay(ra.as_millis() as u64);
    }
    let base = BACKOFF_BASE_MS.saturating_mul(1u64 << attempt.min(20));
    clamp_delay(base)
}

fn clamp_delay(ms: u64) -> Duration {
    Duration::from_millis(ms.clamp(BACKOFF_FLOOR_MS, BACKOFF_CAP_MS))
}

/// Parse a `Retry-After` header as integer seconds (the form the engine emits).
/// HTTP-date forms are ignored (None) — we fall back to exponential backoff.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_matrix() {
        assert!(is_retryable(429));
        assert!(is_retryable(500));
        assert!(is_retryable(503));
        assert!(!is_retryable(400));
        assert!(!is_retryable(404));
        assert!(!is_retryable(422));
    }

    #[test]
    fn backoff_schedule_and_clamps() {
        assert_eq!(backoff(0, None), Duration::from_millis(500));
        assert_eq!(backoff(1, None), Duration::from_millis(1000));
        assert_eq!(backoff(2, None), Duration::from_millis(2000));
        // Cap at 8s.
        assert_eq!(backoff(10, None), Duration::from_millis(8000));
        // Retry-After honored, clamped into [250ms, 8s].
        assert_eq!(
            backoff(0, Some(Duration::from_secs(2))),
            Duration::from_millis(2000)
        );
        assert_eq!(
            backoff(0, Some(Duration::from_millis(50))),
            Duration::from_millis(250)
        );
        assert_eq!(
            backoff(0, Some(Duration::from_secs(30))),
            Duration::from_millis(8000)
        );
    }

    #[test]
    fn errors_never_carry_bodies() {
        assert_eq!(SumError::Http(500).to_string(), "summarizer returned HTTP 500");
        assert_eq!(SumError::Timeout.to_string(), "summarizer request timed out");
    }

    #[test]
    fn skew_detection() {
        let c = SummarizerClient::with_timeout("http://x", Duration::from_secs(1));
        let ok = HealthResponse {
            ok: true,
            protocol: 1,
            version: "daemon-1.0.0".into(),
            model: "qwen3.5-4b-q4_k_m".into(),
            model_loaded: true,
            backend: "cpu".into(),
        };
        assert!(c.check_skew(&ok, "daemon-1.0.0").is_none());
        let skewed = HealthResponse {
            protocol: 2,
            ..ok.clone()
        };
        assert!(c.check_skew(&skewed, "daemon-1.0.0").is_some());
        let older = HealthResponse {
            version: "daemon-0.9.0".into(),
            ..ok
        };
        assert!(c.check_skew(&older, "daemon-1.0.0").is_some());
    }
}
