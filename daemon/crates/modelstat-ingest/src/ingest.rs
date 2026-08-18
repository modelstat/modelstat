//! The IngestClient upload matrix (feature §17.1/§17.2, plan §5 M4) — the
//! never-drop batch shipper's decision logic. Ported from the TS
//! `@modelstat/daemon-core/http` `IngestClient`.
//!
//! The retry matrix is the whole point (feature §21.2): a batch COMMITS only on
//! a 2xx; 401/403 triggers one machine-stable reauth then retries; and
//! **everything else — 400/404/405/413/422 included — HOLDS and retries**. A
//! held batch stalls only its own file's cursor (the scan loop leaves it
//! un-advanced) and drains once the server is healthy; a dropped batch is gone
//! for good. There is no "permanent reject" outcome by construction: the
//! wire-boundary byte clamp ([`modelstat_wire::IngestBatch::clamp`]) plus Rust's
//! inherently well-formed UTF-8 strings mean no poison batch can be produced —
//! so the TS `wellFormedStringify` lone-surrogate fix is a no-op here.
//!
//! The pure helpers here are unit-tested in isolation; [`super::DeviceApi::upload_batch`]
//! wires them to the real HTTP client + reauth hook.

use std::time::Duration;

use serde::Deserialize;

/// Upper bound on retry attempts within a single `upload_batch` call (TS
/// `maxAttempts ?? 3`). Exhausting it returns a HOLD, not a drop — the scan loop
/// retries the same batch next cycle.
pub const INGEST_MAX_ATTEMPTS: usize = 3;

/// Ingest backoff schedule (feature §17.1) — the same `[1, 2.5, 5, 10, 20, 60]s`
/// ladder as the device-request matrix (both port daemon-core `BACKOFF_MS` /
/// `expBackoff`). Kept as its own named constant so drift is a diff, not an
/// incident (plan §6).
pub(crate) const INGEST_BACKOFF_MS: [u64; 6] = [1_000, 2_500, 5_000, 10_000, 20_000, 60_000];

/// The backoff delay for a given (0-based) attempt, clamped to the last rung.
pub(crate) fn ingest_backoff(attempt: usize) -> Duration {
    let i = attempt.min(INGEST_BACKOFF_MS.len() - 1);
    Duration::from_millis(INGEST_BACKOFF_MS[i])
}

/// The server's accepted-batch receipt (feature §17.2). `raw_s3_key` is set only
/// on the `/v1/ingest/raw` (cloud) path. Every field defaults so a 2xx with an
/// opaque/empty body still reads as a commit (TS `res.json().catch(() => ({}))`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct IngestResponse {
    #[serde(default)]
    pub accepted: u64,
    #[serde(default)]
    pub new_sessions: u64,
    #[serde(default)]
    pub updated_sessions: u64,
    #[serde(default)]
    pub batch_id: String,
    #[serde(default)]
    pub raw_s3_key: Option<String>,
}

/// Whose fault a hold is: the wire, or this one batch.
///
/// Both HOLD — nothing is ever dropped, and this does not change the retry
/// ladder by one millisecond. The only question it answers is whether trying a
/// DIFFERENT batch in the same pass is worth it. Without it the drain cannot
/// tell "the server is down" from "the server refuses THIS payload", so it must
/// assume the worst and stop the pass — which is how one unmodelled record kind
/// (`thinking_level_change`, from an OMP transcript) held 98 batches behind it
/// for 14 hours while the daemon reported itself healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldScope {
    /// The transport, or the server as a whole: network error, 5xx, 429, 408,
    /// missing/refused credentials. The next batch would fail identically, so a
    /// pass that sees this should stop and back off.
    Wire,
    /// The server rejected THIS payload on its content (400/413/415/422). Its
    /// siblings are unaffected and must still get their turn — otherwise one
    /// unshippable batch at the head of the queue is a total outage.
    Batch,
}

impl HoldScope {
    /// Classify a non-2xx status. A content rejection is batch-scoped; anything
    /// that could equally befall the next batch is wire-scoped.
    ///
    /// 404/405 are deliberately [`Self::Wire`]: a missing route is a deployment
    /// or proxy fault affecting every batch, not a complaint about this one.
    #[must_use]
    pub fn for_status(status: u16) -> Self {
        match status {
            400 | 413 | 415 | 422 => Self::Batch,
            _ => Self::Wire,
        }
    }
}

/// The outcome of one [`super::DeviceApi::upload_batch`] call. A non-commit is
/// ALWAYS a HOLD, never a permanent drop (feature §21.2): the caller retries the
/// SAME batch next scan cycle, so good data is never discarded. (TS names the
/// non-commit variant `drop`; the contract — there and here — is hold-and-retry,
/// so we name it [`UploadResult::Hold`] to match the spec's language.)
#[derive(Debug, Clone)]
pub enum UploadResult {
    /// The server accepted the batch (2xx). Cursors may advance.
    Commit(IngestResponse),
    /// Not accepted (no token / reauth failed / retries exhausted on any
    /// non-2xx / network). The reason is surfaced in the scan loop's
    /// `Upload failed: <reason>` status. Cursors do NOT advance.
    ///
    /// `scope` says whether the queue may keep moving past it — see
    /// [`HoldScope`]. It never means "drop this".
    Hold { reason: String, scope: HoldScope },
}

impl UploadResult {
    /// True when the batch committed — the only case where cursors advance.
    pub fn is_commit(&self) -> bool {
        matches!(self, UploadResult::Commit(_))
    }
}

/// What to do with an ingest response status. Shared so "what happens on 401" is
/// answered in exactly one place (port of TS `classifyStatus`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadDecision {
    /// 2xx — accepted.
    Commit,
    /// 401/403 — the bearer was revoked/rotated; reauth once, then retry.
    Reauth,
    /// Everything else — HOLD and retry after this delay.
    Backoff(Duration),
}

/// Classify an ingest response status (feature §17.2). 2xx commits; 401/403
/// reauth; **everything else backs off + HOLDS** — the daemon never discards a
/// batch, so an outage or misconfig is a pause, never a gap in the data. Even
/// 400/404/405/413/422 hold (observed live as Cloudflare/WAF/Caddy transients);
/// the cost of guessing "permanent" wrong is unrecoverable data loss.
pub fn classify_status(status: u16, attempt: usize) -> UploadDecision {
    if (200..300).contains(&status) {
        return UploadDecision::Commit;
    }
    if status == 401 || status == 403 {
        return UploadDecision::Reauth;
    }
    UploadDecision::Backoff(ingest_backoff(attempt))
}

/// Parse a `Retry-After` header (delta-seconds form) into a duration; zero if
/// absent, negative, or unparseable (port of TS `retryAfterMs`). The ingest edge
/// sends it on a 429 (over fair-share) and 503 (summarizer down).
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|&secs| secs >= 0)
        .map(|secs| Duration::from_secs(secs as u64))
        .unwrap_or(Duration::ZERO)
}

/// Additive jitter (port of TS `jitter`): keep `base` as a FLOOR (so a server
/// `Retry-After` is always honored) and spread up to +25% so a fleet backing off
/// on the same `Retry-After` doesn't retry in lockstep — the thundering herd.
/// `base == 0` → `0`. Uses a cheap clock-seeded fraction: jitter only needs
/// cross-machine desync, not crypto-grade randomness.
pub fn jitter(base: Duration) -> Duration {
    let ms = base.as_millis() as u64;
    if ms == 0 {
        return Duration::ZERO;
    }
    let extra = (rand_unit() * ms as f64 * 0.25) as u64;
    Duration::from_millis(ms + extra)
}

/// A cheap `[0, 1)` fraction seeded from the sub-second wall clock. Adequate for
/// jitter (fleet desync), deliberately not a general-purpose PRNG — keeps the
/// dependency graph minimal (plan §2).
fn rand_unit() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1_000_000) as f64 / 1_000_000.0
}

/// Flatten a `reqwest::Error` (and its cause chain) into an actionable string —
/// ECONNREFUSED / ENOTFOUND / CERT_HAS_EXPIRED instead of the opaque "error
/// sending request" wrapper (port of TS `describeErrorWithCause`). Surfaced in
/// the HOLD reason so the daemon's `Upload failed: <reason>` is diagnosable.
pub(crate) fn describe_error(e: &reqwest::Error) -> String {
    use std::error::Error;
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(inner) = src {
        out.push_str(": ");
        out.push_str(&inner.to_string());
        src = inner.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_2xx_commits() {
        for s in [200u16, 201, 202, 204, 299] {
            assert_eq!(classify_status(s, 0), UploadDecision::Commit, "status {s}");
        }
    }

    #[test]
    fn classify_401_403_reauth() {
        assert_eq!(classify_status(401, 0), UploadDecision::Reauth);
        assert_eq!(classify_status(403, 0), UploadDecision::Reauth);
    }

    #[test]
    fn classify_everything_else_holds_never_drops() {
        // The never-drop invariant (feature §21.2): even the 4xx a naive client
        // would treat as permanent must HOLD (backoff), not drop.
        for s in [
            400u16, 404, 405, 408, 413, 415, 422, 426, 429, 500, 502, 503,
        ] {
            assert!(
                matches!(classify_status(s, 0), UploadDecision::Backoff(_)),
                "status {s} must HOLD, never drop"
            );
        }
    }

    /// The scope split must never be confused with dropping: every status above
    /// still HOLDS. This only says who else may move on meanwhile.
    #[test]
    fn content_rejections_are_batch_scoped_everything_else_is_wire() {
        for s in [400u16, 413, 415, 422] {
            assert_eq!(
                HoldScope::for_status(s),
                HoldScope::Batch,
                "status {s} rejects THIS payload; the queue behind it must keep moving"
            );
        }
        // A missing route, throttling, an outage or a blip could hit any batch —
        // stopping the pass is correct for these.
        for s in [404u16, 405, 408, 426, 429, 500, 502, 503] {
            assert_eq!(HoldScope::for_status(s), HoldScope::Wire, "status {s}");
        }
    }

    #[test]
    fn backoff_follows_the_1_2_5_ladder() {
        // Names the constant so a schedule change is a diff, not an incident.
        assert_eq!(ingest_backoff(0), Duration::from_millis(1_000));
        assert_eq!(ingest_backoff(1), Duration::from_millis(2_500));
        assert_eq!(ingest_backoff(2), Duration::from_millis(5_000));
        assert_eq!(ingest_backoff(3), Duration::from_millis(10_000));
        assert_eq!(ingest_backoff(4), Duration::from_millis(20_000));
        assert_eq!(ingest_backoff(5), Duration::from_millis(60_000));
        // Clamps to the last rung, never panics on a high attempt.
        assert_eq!(ingest_backoff(99), Duration::from_millis(60_000));
    }

    #[test]
    fn retry_after_parses_delta_seconds() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let mut h = HeaderMap::new();
        assert_eq!(retry_after(&h), Duration::ZERO, "absent → 0");
        h.insert(RETRY_AFTER, HeaderValue::from_static("30"));
        assert_eq!(retry_after(&h), Duration::from_secs(30));
        h.insert(RETRY_AFTER, HeaderValue::from_static("  5 "));
        assert_eq!(retry_after(&h), Duration::from_secs(5), "trimmed");
        h.insert(RETRY_AFTER, HeaderValue::from_static("-3"));
        assert_eq!(retry_after(&h), Duration::ZERO, "negative → 0");
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(
            retry_after(&h),
            Duration::ZERO,
            "http-date form unparsed → 0"
        );
    }

    #[test]
    fn jitter_keeps_base_as_floor_and_caps_at_125_percent() {
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
        for _ in 0..1_000 {
            let j = jitter(Duration::from_millis(1_000));
            assert!(
                j >= Duration::from_millis(1_000),
                "Retry-After is a floor: {j:?} < 1000ms"
            );
            assert!(
                j < Duration::from_millis(1_250),
                "jitter caps at +25%: {j:?} >= 1250ms"
            );
        }
    }

    #[test]
    fn raw_ingest_boolean_accepted_cannot_parse_as_a_count() {
        // /v1/ingest/raw acks with `{"accepted": true}` (a boolean); /v1/ingest
        // reports `{"accepted": <count>}`. The boolean CANNOT deserialize into
        // `IngestResponse.accepted` (u64) — it errors, so a naive
        // `unwrap_or_default()` would read 0. That is exactly why `upload_batch`
        // counts a raw commit by events-sent rather than trusting the raw body
        // (which is why cloud mode reported events_uploaded=0 while every event
        // landed).
        assert!(
            serde_json::from_str::<IngestResponse>(r#"{"accepted": true}"#).is_err(),
            "a boolean `accepted` must NOT silently parse as a count"
        );
        // The non-raw numeric ack still parses fine.
        assert_eq!(
            serde_json::from_str::<IngestResponse>(r#"{"accepted": 42}"#)
                .unwrap()
                .accepted,
            42
        );
    }
}
