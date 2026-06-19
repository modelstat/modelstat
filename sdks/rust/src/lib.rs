//! # modelstat
//!
//! A privacy-first SDK for wrapping the LLM calls your backend already makes and
//! shipping **redacted** usage to modelstat — without adding latency to live
//! requests.
//!
//! The hot path ([`Client::record`]) does nothing but copy your already-in-hand
//! call into a bounded buffer and return. A background worker redacts, batches,
//! and ships off the request path. On overflow the newest record is dropped and
//! a counter increments — your request is never blocked and never grows memory
//! unbounded.
//!
//! ## Modes
//!
//! - **Local daemon (default).** Hand calls to a local modelstat daemon over
//!   loopback; it summarizes with a local Qwen model and ships only redacted
//!   abstracts. Raw text never leaves the machine.
//! - **Remote.** Ship directly to the modelstat server (no local model). With
//!   `raw = true`, send full floor-redacted turns for server-side
//!   summarization.
//!
//! ```no_run
//! # async fn demo() {
//! use modelstat::{Client, Config, LlmCall, TokenUsage};
//!
//! // Org-scoped ingest key binds traffic to your account; remote mode here.
//! let cfg = Config::new("msk_live_…", "raw_sdk_openai")
//!     .with_remote("https://api.modelstat.ai", /* raw */ true);
//! let ms = Client::new(cfg);
//!
//! // ... after your real LLM call returns ...
//! ms.record(
//!     LlmCall::new("openai", "session-or-trace-id")
//!         .model("gpt-x")
//!         .tokens(TokenUsage { input: 800, output: 120, ..Default::default() })
//!         .text("the prompt", "the completion"),
//! );
//!
//! ms.shutdown().await; // flush on the way out
//! # }
//! ```

mod capture;
mod config;
mod redact;
mod transport;
mod wire;
mod worker;

pub use capture::{LlmCall, ToolCallInput};
pub use config::{Config, Mode, RedactionPolicy};
pub use redact::{redact, Redacted};
pub use transport::{FakeTransport, HttpTransport, Transport, TransportError};
pub use wire::{
    BillingMode, EventKind, GitContext, IngestBatch, RawEvent, TokenUsage, ToolCallStatus,
    ToolCallWire,
};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Internal channel message: a captured call, or a drain request with an ack.
pub(crate) enum Msg {
    Call(Box<LlmCall>),
    Drain(oneshot::Sender<()>),
}

/// A cheap, cloneable handle to the SDK. Cloning shares the same buffer and
/// worker, so you can hand a `Client` to every request handler.
#[derive(Clone)]
pub struct Client {
    tx: mpsc::Sender<Msg>,
    dropped: Arc<AtomicU64>,
}

impl Client {
    /// Start the SDK with the default HTTP transport for `cfg.mode`. Must be
    /// called from within a Tokio runtime.
    #[must_use]
    pub fn new(cfg: Config) -> Self {
        let transport: Arc<dyn Transport> = Arc::new(HttpTransport::from_config(&cfg));
        Self::with_transport(cfg, transport)
    }

    /// Start the SDK with a custom [`Transport`] (e.g. [`FakeTransport`] in
    /// tests).
    #[must_use]
    pub fn with_transport(cfg: Config, transport: Arc<dyn Transport>) -> Self {
        let (tx, rx) = mpsc::channel(cfg.buffer_capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        worker::spawn(cfg, rx, transport);
        Self { tx, dropped }
    }

    /// Record a captured call. **Hot path:** a non-blocking move into the
    /// buffer. If the buffer is full the call is dropped and [`Client::dropped`]
    /// increments — the caller is never blocked.
    pub fn record(&self, call: LlmCall) {
        if self.tx.try_send(Msg::Call(Box::new(call))).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Number of calls dropped due to buffer overflow (a backpressure signal).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Flush buffered calls and wait for the worker to ship them.
    pub async fn flush(&self) {
        let (ack, rx) = oneshot::channel();
        if self.tx.send(Msg::Drain(ack)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Flush on the way out. Equivalent to [`Client::flush`]; provided as the
    /// conventional shutdown call.
    pub async fn shutdown(self) {
        self.flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_then_flush_delivers_a_redacted_batch() {
        let cfg = Config::new("msk_test", "raw_sdk_openai").with_device_id("dev_test");
        let fake = Arc::new(FakeTransport::new());
        let ms = Client::with_transport(cfg, fake.clone());

        ms.record(
            LlmCall::new("openai", "sess_1")
                .model("gpt-x")
                .tokens(TokenUsage {
                    input: 100,
                    output: 20,
                    ..Default::default()
                })
                .text("my email is jane@example.com", "done"),
        );
        ms.flush().await;

        let batches = fake.batches();
        assert_eq!(batches.len(), 1);
        let ev = &batches[0].events[0];
        assert_eq!(ev.provider, "openai");
        assert_eq!(ev.tokens.input, 100);
        let excerpt = ev.content_excerpt.as_ref().unwrap();
        assert!(excerpt.contains("[REDACTED:email]"), "got {excerpt:?}");
        assert!(!excerpt.contains("jane@example.com"));
        assert_eq!(ms.dropped(), 0);
    }

    #[tokio::test]
    async fn overflow_drops_newest_and_counts_without_blocking() {
        // Tiny buffer, and never flush, so the buffer fills.
        let mut cfg = Config::new("msk", "raw_sdk_generic").with_device_id("dev_test");
        cfg.buffer_capacity = 2;
        cfg.flush_interval = std::time::Duration::from_secs(3600);
        let fake = Arc::new(FakeTransport::new());
        let ms = Client::with_transport(cfg, fake);

        for _ in 0..50 {
            ms.record(LlmCall::new("openai", "sess_1"));
        }
        // The worker may have pulled a couple, but most overflow — the point is
        // record() never blocked and overflow is counted.
        assert!(
            ms.dropped() > 0,
            "expected some drops, got {}",
            ms.dropped()
        );
    }
}
