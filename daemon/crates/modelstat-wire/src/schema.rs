//! Wire schemas — serde types bit-aligned with `packages/core/src/schemas.ts`
//! and the server's Rust ingest schema (feature §17.3).
//!
//! Conventions that keep this a faithful port of the Zod schemas:
//!   * A Zod `.nullable()` field is required-but-may-be-null: modeled as
//!     `Option<T>` that serializes `null` when `None` (present). `#[serde(default)]`
//!     makes deserialize tolerant of an absent key too.
//!   * A Zod `.optional()` field may be absent: modeled as `Option<T>` with
//!     `skip_serializing_if = "Option::is_none"`, so `None` is omitted (matching
//!     JS `JSON.stringify` dropping `undefined`).
//!   * A Zod `.default(x)` field always materializes on output; `#[serde(default)]`
//!     supplies `x` on input.
//!   * Additive nested blobs the daemon merely passes through (`references`,
//!     `session_metadata`, `session_installs`, heartbeat `stats`) are held as
//!     `serde_json::Value` for lossless round-trip; their full typed ports land
//!     with the enrichment milestone (M4). This is strictly more faithful for
//!     the wire-parity test than a hand re-serialization.
//!
//! Caps live in [`crate::caps`]; [`IngestBatch::clamp`] applies them in UTF-8
//! bytes (the explicit clamp layer, plan D12).

use crate::caps;
use crate::clamp::{clamp_in_place, clamp_opt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Token usage — every field defaults to 0 (Zod `.default(0)`), so all five
/// always serialize.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

/// Git context — all four fields nullable (present, may be null).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitContext {
    #[serde(default)]
    pub remote_url: Option<String>,
    #[serde(default)]
    pub remote_host: Option<String>,
    #[serde(default)]
    pub remote_slug: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

/// Redaction report — three guaranteed counters plus `pf_*` catchall keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionReport {
    #[serde(default)]
    pub secrets_found: u64,
    #[serde(default)]
    pub emails_redacted: u64,
    #[serde(default)]
    pub paths_redacted_absolute: u64,
    /// `.catchall(z.number().int().nonnegative())` — extra `pf_<category>` keys.
    #[serde(flatten)]
    pub extra: BTreeMap<String, u64>,
}

/// One raw event — the canonical per-turn row (feature §17.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawEvent {
    pub source_event_id: String,
    pub ts: String,
    pub kind: String,
    pub agent: String,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    pub session_id: String,
    #[serde(default)]
    pub turn_index: Option<u64>,
    #[serde(default)]
    pub parent_event_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub git: Option<GitContext>,
    #[serde(default)]
    pub tokens: Option<TokenUsage>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub tool_calls: BTreeMap<String, u64>,
    #[serde(default)]
    pub files_touched: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_excerpt: Option<String>,
    /// Chars of the cleaned message text BEFORE paste-elision/truncation
    /// (SPEC 0005) — "was this cut / how big was the real prompt" as a stored
    /// fact. Only set when `content_excerpt` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<Value>,
    #[serde(default)]
    pub source_file: Option<String>,
    #[serde(default)]
    pub source_byte_offset: Option<u64>,
}

impl RawEvent {
    /// Clamp every bounded string to its UTF-8-byte cap (feature §17.2/§17.3).
    pub fn clamp(&mut self) {
        clamp_opt(&mut self.model, caps::MODEL_MAX);
        clamp_in_place(&mut self.session_id, caps::SESSION_ID_MAX);
        for f in &mut self.files_touched {
            clamp_in_place(f, caps::FILES_TOUCHED_ITEM_MAX);
        }
        self.files_touched.truncate(caps::FILES_TOUCHED_COUNT_MAX);
        clamp_opt(&mut self.content_excerpt, caps::CONTENT_EXCERPT_MAX);
        clamp_opt(&mut self.source_file, caps::SOURCE_FILE_MAX);
    }
}

/// A daemon-emitted taxonomy tag hint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaxonomyHintRooted {
    pub root_key: String,
    pub name: String,
    #[serde(default = "default_tag_confidence")]
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn default_tag_confidence() -> f64 {
    0.7
}

/// Privacy-preserving per-segment behavioral signal (counts/ratios only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentBehavior {
    #[serde(default)]
    pub user_turns: u64,
    #[serde(default)]
    pub correction_count: u64,
    #[serde(default)]
    pub frustration: f64,
}

/// A daemon-emitted segment — the sync unit (feature §17.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub segment_id: String,
    pub session_id: String,
    pub agent: String,
    pub started_at: String,
    pub ended_at: String,
    /// `abstract` is a Rust keyword — the raw identifier serializes as
    /// `"abstract"` on the wire (serde strips the `r#`).
    pub r#abstract: String,
    pub tokens: TokenUsage,
    #[serde(default)]
    pub tags: Vec<TaxonomyHintRooted>,
    pub redaction: RedactionReport,
    pub source_event_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abstract_embedding: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<SegmentBehavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_intent: Option<String>,
}

impl Segment {
    pub fn clamp(&mut self) {
        clamp_in_place(&mut self.segment_id, caps::SEGMENT_ID_MAX);
        clamp_in_place(&mut self.session_id, caps::SESSION_ID_MAX);
        clamp_in_place(&mut self.r#abstract, caps::ABSTRACT_MAX);
        for t in &mut self.tags {
            clamp_in_place(&mut t.root_key, caps::TAG_ROOT_KEY_MAX);
            clamp_in_place(&mut t.name, caps::TAG_NAME_MAX);
            clamp_opt(&mut t.reason, caps::TAG_REASON_MAX);
        }
        self.tags.truncate(caps::TAGS_COUNT_MAX);
        self.source_event_ids
            .truncate(caps::SOURCE_EVENT_IDS_COUNT_MAX);
        clamp_opt(&mut self.user_intent, caps::USER_INTENT_MAX);
    }
}

/// One script summary inside a [`ToolAction`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptSummary {
    pub token: String,
    pub summary: String,
}

/// On-device action decomposition of a tool call — strict (unknown keys
/// rejected, mirroring Zod `.strict()`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolAction {
    pub surface: String,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub qualifiers: Vec<String>,
    #[serde(default)]
    pub param_shape: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub r#abstract: Option<String>,
    #[serde(default)]
    pub command_redacted: Option<String>,
    #[serde(default)]
    pub scripts: Vec<ScriptSummary>,
    #[serde(default)]
    pub confidence: f64,
    pub extractor: String,
}

impl ToolAction {
    pub fn clamp(&mut self) {
        clamp_in_place(&mut self.surface, caps::TA_SURFACE_MAX);
        clamp_opt(&mut self.executable, caps::TA_EXECUTABLE_MAX);
        clamp_opt(&mut self.action, caps::TA_ACTION_MAX);
        clamp_opt(&mut self.object, caps::TA_OBJECT_MAX);
        for q in &mut self.qualifiers {
            clamp_in_place(q, caps::TA_QUALIFIER_ITEM_MAX);
        }
        self.qualifiers.truncate(caps::TA_QUALIFIERS_COUNT_MAX);
        clamp_opt(&mut self.param_shape, caps::TA_PARAM_SHAPE_MAX);
        for k in &mut self.keywords {
            clamp_in_place(k, caps::TA_KEYWORD_ITEM_MAX);
        }
        self.keywords.truncate(caps::TA_KEYWORDS_COUNT_MAX);
        clamp_opt(&mut self.r#abstract, caps::TA_ABSTRACT_MAX);
        clamp_opt(&mut self.command_redacted, caps::TA_COMMAND_REDACTED_MAX);
        for s in &mut self.scripts {
            clamp_in_place(&mut s.token, caps::TA_SCRIPT_TOKEN_MAX);
            clamp_in_place(&mut s.summary, caps::TA_SCRIPT_SUMMARY_MAX);
        }
        self.scripts.truncate(caps::TA_SCRIPTS_COUNT_MAX);
        clamp_in_place(&mut self.extractor, caps::TA_EXTRACTOR_MAX);
    }
}

/// One tool invocation (feature §17.3, §7.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallWire {
    pub external_call_id: String,
    pub session_id: String,
    pub source_event_id: String,
    #[serde(default)]
    pub segment_id: Option<String>,
    pub agent: String,
    pub server: String,
    pub name: String,
    #[serde(default)]
    pub turn_index: Option<u64>,
    pub call_index: u64,
    pub started_at: String,
    #[serde(default)]
    pub ended_at: Option<String>,
    pub status: String,
    pub args_hash: String,
    pub signature_hash: String,
    pub args_bytes: u64,
    pub result_bytes: u64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub action: Option<ToolAction>,
}

impl ToolCallWire {
    pub fn clamp(&mut self) {
        clamp_in_place(&mut self.external_call_id, caps::EXTERNAL_CALL_ID_MAX);
        clamp_in_place(&mut self.session_id, caps::SESSION_ID_MAX);
        clamp_opt(&mut self.segment_id, caps::SEGMENT_ID_MAX);
        clamp_in_place(&mut self.server, caps::SERVER_MAX);
        clamp_in_place(&mut self.name, caps::NAME_MAX);
        clamp_in_place(&mut self.args_hash, caps::ARGS_HASH_MAX);
        clamp_in_place(&mut self.signature_hash, caps::SIGNATURE_HASH_MAX);
        clamp_opt(&mut self.model, caps::MODEL_MAX);
        if let Some(a) = &mut self.action {
            a.clamp();
        }
    }
}

/// The batch the daemon ships to `/v1/ingest` (feature §17.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestBatch {
    pub batch_id: String,
    pub device_id: String,
    pub daemon_version: String,
    pub events: Vec<RawEvent>,
    #[serde(default)]
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_installs: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_titles: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarizer_mode: Option<String>,
    /// Where this batch was REDACTED — `local` (on the device, the default and the
    /// only mode where nothing unscrubbed can leave), `cloud`, or `self-hosted`.
    /// Separate from `summarizer_mode` because the two are separate questions, and
    /// recorded per batch so "was this scrubbed on the box?" is answerable later
    /// rather than inferred from a setting that may since have changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redactor_mode: Option<String>,
}

impl IngestBatch {
    /// Apply every UTF-8-byte cap across the whole batch (the wire-boundary
    /// clamp, feature §17.2). Array cardinality caps are enforced too so no
    /// permanently-rejectable batch can be produced.
    pub fn clamp(&mut self) {
        clamp_in_place(&mut self.daemon_version, caps::DAEMON_VERSION_MAX);
        for e in &mut self.events {
            e.clamp();
        }
        for s in &mut self.segments {
            s.clamp();
        }
        for t in &mut self.tool_calls {
            t.clamp();
        }
        if let Some(titles) = &mut self.session_titles {
            for v in titles.values_mut() {
                clamp_in_place(v, caps::SESSION_TITLE_MAX);
            }
        }
        self.events.truncate(caps::EVENTS_COUNT_MAX);
        self.segments.truncate(caps::SEGMENTS_COUNT_MAX);
        self.tool_calls.truncate(caps::TOOL_CALLS_COUNT_MAX);
    }
}

/// Heartbeat payload (feature §6.4/§17.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    pub device_id: String,
    pub status: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub progress_done: u64,
    #[serde(default)]
    pub progress_total: u64,
    #[serde(default)]
    pub queue_size: u64,
    #[serde(default)]
    pub stats: Value,
    #[serde(default)]
    pub last_event_at: Option<String>,
    pub daemon_version: String,
}

impl HeartbeatPayload {
    pub fn clamp(&mut self) {
        clamp_opt(&mut self.message, caps::HEARTBEAT_MESSAGE_MAX);
        clamp_in_place(&mut self.daemon_version, caps::DAEMON_VERSION_MAX);
    }
}

/// The device fingerprint shared by register + heartbeat (feature §4). The
/// `machine_id` is the server's dedupe anchor and must be byte-identical
/// everywhere it is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub hostname: String,
    pub os_family: String,
    pub os_version: String,
    pub arch: String,
    pub daemon: String,
    pub daemon_version: String,
    pub machine_id: String,
}

/// Register-door body — `POST /v1/tokens` (feature §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub device_uuid: String,
    pub fingerprint: Fingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenusage_defaults_all_zero_and_roundtrips() {
        let tu: TokenUsage = serde_json::from_str("{}").unwrap();
        assert_eq!(tu, TokenUsage::default());
        let json = serde_json::to_string(&tu).unwrap();
        assert!(json.contains("\"reasoning\":0"));
    }

    #[test]
    fn tool_action_is_strict() {
        // Unknown key must be rejected (Zod `.strict()` parity).
        let err = serde_json::from_str::<ToolAction>(
            r#"{"surface":"shell","extractor":"shell.v3","bogus":1}"#,
        );
        assert!(err.is_err());
    }

    #[test]
    fn nullable_serializes_null_optional_omits() {
        let ev = RawEvent {
            source_event_id: "evt_x".into(),
            ts: "2026-01-01T00:00:00Z".into(),
            kind: "user_message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s".into(),
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: None,
            duration_ms: None,
            tool_calls: BTreeMap::new(),
            files_touched: vec![],
            content_excerpt: None,
            content_bytes: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
        };
        let v: Value = serde_json::to_value(&ev).unwrap();
        // nullable → present as null
        assert!(v.get("model").unwrap().is_null());
        assert!(v.get("source_file").unwrap().is_null());
        // optional → omitted
        assert!(v.get("content_excerpt").is_none());
        assert!(v.get("content_bytes").is_none());
        // required → always present, even at its most conservative value
        // default → materialized
        assert!(v.get("tool_calls").unwrap().is_object());
        assert!(v.get("files_touched").unwrap().is_array());
    }

    #[test]
    fn clamp_truncates_abstract_bytes() {
        let mut seg = Segment {
            segment_id: "seg_x".into(),
            session_id: "s".into(),
            agent: "claude_code".into(),
            started_at: "t".into(),
            ended_at: "t".into(),
            r#abstract: "字".repeat(300), // 900 bytes > 512
            tokens: TokenUsage::default(),
            tags: vec![],
            redaction: RedactionReport::default(),
            source_event_ids: vec![],
            abstract_embedding: None,
            behavior: None,
            user_intent: None,
        };
        seg.clamp();
        assert!(seg.r#abstract.len() <= caps::ABSTRACT_MAX);
    }
}
