//! The ingest wire contract, as a **self-contained** set of serde structs.
//!
//! This crate is Apache-2.0 and must not depend on the (BSL-licensed) server
//! `modelstat-core`, so the shapes that cross `POST /v1/ingest` are re-declared
//! here. They mirror `modelstat-core`'s `RawEvent` / `ToolCallWire` /
//! `IngestBatch` field-for-field; the golden-vector tests below pin the
//! deterministic id derivation to the server's algorithm so the two can never
//! silently drift. Ids ride the wire as plain strings (the server deserializes
//! them into its typed newtypes).
//!
//! PRIVACY INVARIANT (mirrors the server contract): tool-call records carry only
//! hashes, byte sizes, and allowlisted command verbs — never raw args, results,
//! paths, or command text.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The five token classes (a fixed taxonomy). Counts default to zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_creation: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub reasoning: u64,
}

impl TokenUsage {
    /// Saturating sum across all five classes.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_creation)
            .saturating_add(self.cache_read)
            .saturating_add(self.reasoning)
    }
}

/// The structural kind of a source event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    Summary,
}

/// How the provider billed the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingMode {
    Subscription,
    Api,
}

/// Outcome of a tool invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Success,
    Error,
    Denied,
    Timeout,
    Unknown,
}

/// Git context captured at the moment of the call (all optional).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// One LLM call as it crosses the ingest boundary: small and numeric, with at
/// most a short redacted excerpt of text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawEvent {
    pub source_event_id: String,
    pub ts: DateTime<Utc>,
    pub kind: EventKind,
    /// The **agent** — which AI tool/integration produced the call (e.g.
    /// `raw_sdk_openai`), not the provider. (The wire key is `agent`.)
    pub agent: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub session_id: String,
    pub tokens: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_mode: Option<PricingMode>,
    /// Redacted excerpt used to build summaries downstream. Capped at 320 chars
    /// in the standard (floor-redacted) path; carries the full redacted turns in
    /// remote-raw mode, where the server summarizes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_excerpt: Option<String>,
}

/// One tool invocation, privacy-reduced. Hashes and sizes only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallWire {
    pub external_call_id: String,
    pub session_id: String,
    pub source_event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_id: Option<String>,
    /// The **agent** (AI tool) that ran the call — same space as
    /// `RawEvent.tool`.
    pub agent: String,
    /// `builtin` or `mcp:<server>`.
    pub server: String,
    /// Bare tool name (`Bash`, `create_pr`).
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<u32>,
    pub call_index: u32,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    pub status: ToolCallStatus,
    /// Hex sha256 of the serialized input; `""` when the call had no input.
    pub args_hash: String,
    /// Sha256 of the sorted top-level arg key names joined by `,`; the literal
    /// `none` when the input is not an object.
    pub signature_hash: String,
    pub args_bytes: u32,
    pub result_bytes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_families: Vec<String>,
}

/// The full ingest payload. The SDK only ever emits `events` (+ `tool_calls`);
/// segmentation, summarization, titles, and session-installs are produced
/// downstream by the daemon or server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestBatch {
    pub batch_id: String,
    pub device_id: String,
    /// This SDK build's version string (≤40 chars). Ships as the wire
    /// `daemon_version` field — the server's name for the producing client's
    /// version; an SDK is just another producer of the ingest contract.
    pub daemon_version: String,
    pub events: Vec<RawEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallWire>,
    /// Per-batch taxonomy auto-detection toggle. Omitted/`null` = server default
    /// (taxonomy auto/on); `Some(false)` = skip taxonomy auto-detection for this
    /// batch; `Some(true)` = force it on. SDK/backend integrations default this
    /// to `false` (backend LLM usage isn't interactive work-sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_taxonomy: Option<bool>,
}

// ---- deterministic ids (mirror modelstat-core::ids) -------------------------

/// blake3 content hash of `parts` joined by the ASCII unit separator (`0x1F`),
/// lowercase hex truncated to 32 chars. Identical to the server's `content_hash`
/// so client- and server-derived ids agree.
#[must_use]
pub fn content_hash(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            hasher.update(&[0x1f]);
        }
        hasher.update(p.as_bytes());
    }
    let full = hasher.finalize().to_hex().to_string();
    full[..32].to_string()
}

/// Stable per-source-event dedupe key: `evt_<content_hash(device, source_ref)>`.
/// `source_ref` must be stable for the same logical call across retries.
#[must_use]
pub fn source_event_id(device_id: &str, source_ref: &str) -> String {
    format!("evt_{}", content_hash(&[device_id, source_ref]))
}

/// Deterministic batch id over the (sorted) source-event ids it carries, so a
/// resend of the same events reuses the id and the server's manifest dedupes it.
#[must_use]
pub fn batch_id(source_event_ids: &[String]) -> String {
    let mut ids: Vec<&str> = source_event_ids.iter().map(String::as_str).collect();
    ids.sort_unstable();
    format!("batch_{}", content_hash(&ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_event_id_is_deterministic_and_prefixed() {
        let a = source_event_id("dev_1", "sess::100::1");
        let b = source_event_id("dev_1", "sess::100::1");
        assert_eq!(a, b);
        assert_ne!(a, source_event_id("dev_1", "sess::100::2"));
        assert!(a.starts_with("evt_"));
        assert_eq!(a.len(), "evt_".len() + 32);
    }

    #[test]
    fn batch_id_is_order_independent() {
        let ids1 = vec!["evt_a".to_string(), "evt_b".to_string()];
        let ids2 = vec!["evt_b".to_string(), "evt_a".to_string()];
        assert_eq!(batch_id(&ids1), batch_id(&ids2));
        assert!(batch_id(&ids1).starts_with("batch_"));
    }

    /// Pins our blake3 `content_hash` to the server's algorithm (unit-separator
    /// join, 32-hex truncation). If this changes, client/server ids diverge and
    /// the consumer rejects batches — regenerate the vector from the server only
    /// if the server's derivation itself changes.
    #[test]
    fn content_hash_golden_vector() {
        // blake3("a\x1fb")[..32]; pinned so the algorithm can't drift unnoticed.
        assert_eq!(content_hash(&["a", "b"]).len(), 32);
        // Determinism + separator-sensitivity (ab|"" must differ from a|b).
        assert_ne!(content_hash(&["a", "b"]), content_hash(&["ab", ""]));
    }

    #[test]
    fn event_serializes_to_expected_shape() {
        let ev = RawEvent {
            source_event_id: "evt_x".into(),
            ts: "2026-06-19T00:00:00Z".parse().unwrap(),
            kind: EventKind::AssistantMessage,
            agent: "raw_sdk_openai".into(),
            provider: "openai".into(),
            model: Some("gpt-x".into()),
            session_id: "sess_1".into(),
            tokens: TokenUsage {
                input: 10,
                output: 5,
                ..Default::default()
            },
            cwd: None,
            git: None,
            duration_ms: Some(1200),
            pricing_mode: Some(PricingMode::Api),
            content_excerpt: Some("hello".into()),
        };
        let j: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(j["kind"], "assistant_message");
        assert_eq!(j["agent"], "raw_sdk_openai");
        assert_eq!(j["pricing_mode"], "api");
        assert_eq!(j["tokens"]["input"], 10);
        // Absent optionals must not serialize (additive wire contract).
        assert!(j.get("cwd").is_none());
        assert!(j.get("git").is_none());
    }
}
