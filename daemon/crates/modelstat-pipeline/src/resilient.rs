//! The resilient summarizer — hold-and-retry, NEVER degrade (feature §9.4, the
//! no-silent-degradation override of the spec's extractive fallback).
//!
//! When the engine is down / loading / unreachable / erroring, the collector
//! produces NO abstract (never a lesser/extractive one) and drops NO session: it
//! marks the engine unavailable **loudly** (once), HOLDS the work, and retries
//! with a 60s cooldown until a real LLM abstract can be produced. Empty model
//! output is a real error (retry it), never a fallback trigger.
//!
//! This wraps the protocol client's own ≤3-attempt retry (§9) with the coarser
//! hold+cooldown state machine; the layer above (the scan/batch loop, M4) flushes
//! held sessions when [`ResilientSummarizer::is_available`] flips back.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use modelstat_sumclient::{CompleteRequest, SumError, SummarizerClient};

/// The cooldown between retry attempts while the engine is down (§9.4). A dead or
/// loading engine answers connection-refused / 503 fast, so nothing heavyweight
/// is at stake in the collector.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

/// One completion, over the summarizer protocol. Any error (transport, 5xx, 503,
/// empty output) means "engine unavailable" to the resilient layer — never a cue
/// to degrade.
pub trait Summarizer {
    #[allow(async_fn_in_trait)]
    async fn complete(&self, req: &CompleteRequest) -> Result<String, SumError>;
}

impl Summarizer for SummarizerClient {
    async fn complete(&self, req: &CompleteRequest) -> Result<String, SumError> {
        SummarizerClient::complete(self, req).await
    }
}

/// The outcome of a resilient summarize attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizeOutcome {
    /// A real LLM abstract.
    Done(String),
    /// The engine is unavailable — the work is HELD (never degraded, never
    /// dropped), to be retried once the engine recovers.
    Held,
}

struct AvailState {
    /// `Some(t)` while unavailable, where `t` is the last failed attempt.
    unavailable_since: Option<Instant>,
}

/// Wraps a [`Summarizer`] with the hold-and-retry / self-heal state machine.
pub struct ResilientSummarizer<S> {
    summarizer: S,
    cooldown: Duration,
    state: Mutex<AvailState>,
}

impl<S: Summarizer> ResilientSummarizer<S> {
    pub fn new(summarizer: S) -> Self {
        Self::with_cooldown(summarizer, DEFAULT_COOLDOWN)
    }

    pub fn with_cooldown(summarizer: S, cooldown: Duration) -> Self {
        Self {
            summarizer,
            cooldown,
            state: Mutex::new(AvailState {
                unavailable_since: None,
            }),
        }
    }

    /// True unless the engine is currently marked unavailable. Surfaced in
    /// `status` + the heartbeat; the batch loop flushes held work when it flips
    /// back to true.
    pub fn is_available(&self) -> bool {
        self.state.lock().unwrap().unavailable_since.is_none()
    }

    /// The loud status line while the engine is down (§9.4), else None.
    pub fn status_message(&self) -> Option<String> {
        if self.is_available() {
            None
        } else {
            Some("summarizer unavailable — summaries held, retrying".to_string())
        }
    }

    /// Attempt a summary. Success (non-empty text) → `Done`. ANY failure (engine
    /// error / empty output / still in cooldown) → `Held` — the caller must NOT
    /// complete the session and must retry later, never emit a degraded abstract.
    pub async fn summarize(&self, req: &CompleteRequest) -> SummarizeOutcome {
        if !self.should_attempt() {
            return SummarizeOutcome::Held; // within cooldown — hold without hammering
        }
        match self.summarizer.complete(req).await {
            Ok(text) if !text.trim().is_empty() => {
                self.mark_available();
                SummarizeOutcome::Done(text)
            }
            // Empty output is a REAL error (retry it), never a fallback trigger.
            Ok(_) => {
                self.mark_unavailable("summariser returned empty output");
                SummarizeOutcome::Held
            }
            Err(e) => {
                self.mark_unavailable(&e.to_string());
                SummarizeOutcome::Held
            }
        }
    }

    fn should_attempt(&self) -> bool {
        match self.state.lock().unwrap().unavailable_since {
            None => true,
            Some(t) => t.elapsed() >= self.cooldown,
        }
    }

    fn mark_unavailable(&self, reason: &str) {
        let mut s = self.state.lock().unwrap();
        if s.unavailable_since.is_none() {
            // Loud, exactly once on the transition to down (§9.4 / §21.12).
            eprintln!("modelstat: ⚠ summariser unavailable — summaries HELD, retrying ({reason})");
        }
        s.unavailable_since = Some(Instant::now());
    }

    fn mark_available(&self) {
        let mut s = self.state.lock().unwrap();
        if s.unavailable_since.is_some() {
            eprintln!("modelstat: ✓ summariser recovered — flushing held sessions");
        }
        s.unavailable_since = None;
    }
}

/// A boot-time preflight result (§9.4): whether the engine answered a smoke
/// completion, plus a human status line for `status`/logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    pub available: bool,
    pub message: String,
}

/// Preflight the summarizer with one smoke completion (§9.4 — "GET /healthz then
/// one smoke completion"). A non-empty reply ⇒ available, with a ≤60-char sample;
/// anything else ⇒ a loud "unavailable — held, retrying" status. Reports; never
/// throws, never blocks boot. `engine_label` is e.g. "local summarizer
/// (Qwen3.5-4B)" or "self-hosted summarizer at <url>".
pub async fn preflight<S: Summarizer>(engine_label: &str, summarizer: &S) -> PreflightReport {
    let req = CompleteRequest {
        system: "You are a health check. Reply with the single word: ok.".to_string(),
        user: "ok".to_string(),
        temperature: 0.0,
        max_tokens: 16,
        top_k: None,
    };
    match summarizer.complete(&req).await {
        Ok(text) if !text.trim().is_empty() => {
            let sample: String = text.trim().chars().take(60).collect();
            PreflightReport {
                available: true,
                message: format!("{engine_label} — \"{sample}\""),
            }
        }
        _ => PreflightReport {
            available: false,
            message: "summarizer unavailable — summaries held, retrying".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        fail_remaining: AtomicUsize,
        calls: AtomicUsize,
        reply: String,
        empty: bool,
    }

    impl Fake {
        fn fail_then_ok(n: usize) -> Self {
            Self {
                fail_remaining: AtomicUsize::new(n),
                calls: AtomicUsize::new(0),
                reply: "a real LLM abstract".into(),
                empty: false,
            }
        }
        fn always_empty() -> Self {
            Self {
                fail_remaining: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                reply: String::new(),
                empty: true,
            }
        }
    }

    impl Summarizer for Fake {
        async fn complete(&self, _req: &CompleteRequest) -> Result<String, SumError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.empty {
                return Ok(String::new());
            }
            if self.fail_remaining.load(Ordering::SeqCst) > 0 {
                self.fail_remaining.fetch_sub(1, Ordering::SeqCst);
                return Err(SumError::Http(503));
            }
            Ok(self.reply.clone())
        }
    }

    fn req() -> CompleteRequest {
        CompleteRequest {
            system: "s".into(),
            user: "u".into(),
            temperature: 0.2,
            max_tokens: 1024,
            top_k: Some(3),
        }
    }

    #[tokio::test]
    async fn healthy_engine_completes_immediately() {
        let r = ResilientSummarizer::with_cooldown(Fake::fail_then_ok(0), Duration::ZERO);
        assert_eq!(
            r.summarize(&req()).await,
            SummarizeOutcome::Done("a real LLM abstract".into())
        );
        assert!(r.is_available());
    }

    #[tokio::test]
    async fn preflight_reports_up_and_down() {
        let up = preflight("local summarizer (Qwen3.5-4B)", &Fake::fail_then_ok(0)).await;
        assert!(up.available);
        assert!(up.message.contains("local summarizer (Qwen3.5-4B)"));
        // Always-failing engine → unavailable, loud status.
        let down = preflight("local summarizer", &Fake::fail_then_ok(usize::MAX)).await;
        assert!(!down.available);
        assert!(down.message.contains("held, retrying"));
    }

    #[tokio::test]
    async fn holds_while_down_then_flushes_real_abstract_on_recovery() {
        let r = ResilientSummarizer::with_cooldown(Fake::fail_then_ok(2), Duration::ZERO);
        // Two failures → HELD, never degraded, loudly unavailable.
        assert_eq!(r.summarize(&req()).await, SummarizeOutcome::Held);
        assert!(!r.is_available());
        assert!(r.status_message().is_some());
        assert_eq!(r.summarize(&req()).await, SummarizeOutcome::Held);
        // Recovery → the REAL LLM abstract, available again.
        assert_eq!(
            r.summarize(&req()).await,
            SummarizeOutcome::Done("a real LLM abstract".into())
        );
        assert!(r.is_available());
        assert!(r.status_message().is_none());
    }

    #[tokio::test]
    async fn empty_output_is_held_never_degraded() {
        let r = ResilientSummarizer::with_cooldown(Fake::always_empty(), Duration::ZERO);
        // Empty output is a real error: held, not a degraded/blank abstract.
        assert_eq!(r.summarize(&req()).await, SummarizeOutcome::Held);
        assert!(!r.is_available());
    }

    #[tokio::test]
    async fn cooldown_holds_without_hammering_the_engine() {
        // A long cooldown: after the first failure, further attempts are held
        // WITHOUT calling the engine again (no hammering a dead engine).
        let fake = Fake::fail_then_ok(1);
        let r = ResilientSummarizer::with_cooldown(fake, Duration::from_secs(3600));
        assert_eq!(r.summarize(&req()).await, SummarizeOutcome::Held);
        let calls_after_first = engine_calls(&r);
        // Second attempt within cooldown → Held, and the engine was NOT called.
        assert_eq!(r.summarize(&req()).await, SummarizeOutcome::Held);
        assert_eq!(engine_calls(&r), calls_after_first);
    }

    fn engine_calls(r: &ResilientSummarizer<Fake>) -> usize {
        r.summarizer.calls.load(Ordering::SeqCst)
    }
}
