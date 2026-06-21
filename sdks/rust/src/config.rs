//! SDK configuration: where to ship, how to authenticate, how hard to redact,
//! and how the background worker batches.

use std::time::Duration;

/// Where the SDK ships captured calls.
#[derive(Debug, Clone)]
pub enum Mode {
    /// Hand off to a local modelstat daemon over loopback. The daemon summarizes
    /// with its local Qwen model and ships only redacted abstracts to the
    /// server. This is the default — raw text never leaves the machine.
    LocalDaemon {
        /// The daemon's loopback ingest URL.
        url: String,
    },
    /// Ship directly to the modelstat server (no local daemon / no local model).
    Remote {
        /// Base URL, e.g. `https://api.modelstat.ai`.
        base_url: String,
        /// When `true`, send full (still floor-redacted) turns to
        /// `/v1/ingest/raw` for **server-side** summarization. When `false`,
        /// send only the floor-redacted ≤320-char excerpt to `/v1/ingest`.
        raw: bool,
    },
}

impl Mode {
    /// The default local daemon loopback URL.
    pub const DEFAULT_DAEMON_URL: &'static str = "http://127.0.0.1:4319/v1/ingest";

    /// Resolve the concrete POST endpoint for this mode.
    #[must_use]
    pub fn endpoint(&self) -> String {
        match self {
            Mode::LocalDaemon { url } => url.clone(),
            Mode::Remote {
                base_url,
                raw: false,
            } => format!("{}/v1/ingest", base_url.trim_end_matches('/')),
            Mode::Remote {
                base_url,
                raw: true,
            } => format!("{}/v1/ingest/raw", base_url.trim_end_matches('/')),
        }
    }
}

/// How hard to scrub text before it leaves the SDK process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionPolicy {
    /// Run the privacy floor (secrets + email + absolute paths). The default,
    /// and the floor that even "raw" mode keeps.
    Floor,
    /// Skip in-process redaction entirely. Only valid when shipping to a trusted
    /// local daemon that will redact, or under an explicit raw-data contract.
    None,
}

/// SDK configuration. Build with [`Config::new`] then adjust fields, or use the
/// `with_*` setters.
#[derive(Debug, Clone)]
pub struct Config {
    /// Stable device/service identifier (`dev_…`). Should be stable per host so
    /// dedupe keys are stable across restarts.
    pub device_id: String,
    /// The **agent** label for every record — which AI tool/integration the
    /// user used (e.g. `raw_sdk_openai`, `raw_sdk_anthropic`, `raw_sdk_generic`;
    /// an `AGENTS` value). Ships as the wire `agent` field.
    pub agent: String,
    /// This client build's version (≤40 chars). Ships as the wire
    /// `daemon_version` field — the *producer's* version (daemon or SDK), not
    /// the agent's.
    pub client_version: String,
    /// Bearer credential: an org-scoped ingest key (`msk_…`) or a device secret.
    pub ingest_key: String,
    /// Where to ship.
    pub mode: Mode,
    /// In-process redaction policy.
    pub redaction: RedactionPolicy,
    /// Bounded in-memory buffer between the hot path and the worker. On
    /// overflow the newest record is dropped and the dropped-counter increments
    /// — the live request is never blocked.
    pub buffer_capacity: usize,
    /// Flush the buffer at least this often.
    pub flush_interval: Duration,
    /// Flush eagerly once this many records are buffered.
    pub flush_max_batch: usize,
    /// Whether the server should run taxonomy auto-detection on batches from
    /// this client. Ships as the wire `auto_taxonomy` field. Defaults to
    /// `false` for SDK/backend integrations — backend LLM usage isn't
    /// interactive work-sessions, so taxonomy is **off by default**; set it to
    /// `true` to opt in.
    pub auto_taxonomy: bool,
}

impl Config {
    /// A config with sane defaults: local-daemon mode, floor redaction, a 4096-
    /// slot buffer, a 2s flush interval, and 256-record batches.
    #[must_use]
    pub fn new(ingest_key: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            device_id: "dev_sdk".into(),
            agent: agent.into(),
            client_version: concat!("rust-sdk/", env!("CARGO_PKG_VERSION")).into(),
            ingest_key: ingest_key.into(),
            mode: Mode::LocalDaemon {
                url: Mode::DEFAULT_DAEMON_URL.into(),
            },
            redaction: RedactionPolicy::Floor,
            buffer_capacity: 4096,
            flush_interval: Duration::from_secs(2),
            flush_max_batch: 256,
            auto_taxonomy: false,
        }
    }

    /// Ship directly to the modelstat server instead of a local daemon.
    /// `raw = true` opts into server-side summarization of full (floor-redacted)
    /// turns.
    #[must_use]
    pub fn with_remote(mut self, base_url: impl Into<String>, raw: bool) -> Self {
        self.mode = Mode::Remote {
            base_url: base_url.into(),
            raw,
        };
        self
    }

    /// Override the device id.
    #[must_use]
    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = device_id.into();
        self
    }

    /// Whether this mode sends full (untruncated) redacted turns for
    /// server-side summarization.
    #[must_use]
    pub(crate) fn sends_full_turns(&self) -> bool {
        matches!(self.mode, Mode::Remote { raw: true, .. })
    }
}
