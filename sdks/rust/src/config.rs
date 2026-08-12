//! SDK configuration: where to ship, how to authenticate, how hard to redact,
//! and how the background worker batches.

use std::collections::BTreeMap;
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
    /// The integration's own name — which app/service this SDK instance
    /// instruments (e.g. `checkout-api`, `modelstat-pipeline`). This is the
    /// name your usage appears under in the dashboard: the server registers a
    /// real device per (org, app) and a real provider account per (provider,
    /// app) from it, so SDK traffic is attributed at ingest like any other —
    /// no shared placeholder entities. Defaults to the running binary's name;
    /// set it explicitly when several services share one binary name.
    pub app: String,
    /// Stable device/service identifier (`dev_…`). Should be stable per host so
    /// dedupe keys are stable across restarts. Leave the default: the server
    /// derives the real device identity from (org, [`Config::app`]); this field
    /// only matters for advanced setups that pre-registered their own device.
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
    /// Constant attribution tags applied to **every** call (e.g.
    /// `environment=prod`, `service=checkout`). These form the lowest-priority
    /// layer: a per-call tag with the same key wins. Capped before send (≤16
    /// entries; keys ≤64 chars; values ≤256 chars). Empty by default.
    pub metadata: BTreeMap<String, String>,
}

impl Config {
    /// A config with sane defaults: local-daemon mode, floor redaction, a 4096-
    /// slot buffer, a 2s flush interval, and 256-record batches.
    #[must_use]
    pub fn new(ingest_key: impl Into<String>, agent: impl Into<String>) -> Self {
        Self {
            app: default_app_name(),
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
            metadata: BTreeMap::new(),
        }
    }

    /// Name the integration this SDK instance instruments — the app your usage
    /// appears under in the dashboard (see [`Config::app`]). Returns `self`
    /// for chaining.
    #[must_use]
    pub fn with_app(mut self, app: impl Into<String>) -> Self {
        self.app = app.into();
        self
    }

    /// Add a constant attribution tag applied to every call (overwriting any
    /// previous value for `key`). Returns `self` for chaining.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
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

/// The running binary's file name — the [`Config::app`] default. An app name
/// is required for the server to register the integration's own real entities,
/// and the binary name is the one always-available honest answer; `sdk-app`
/// only when even that can't be read (unusual embedders).
fn default_app_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sdk-app".to_string())
}
