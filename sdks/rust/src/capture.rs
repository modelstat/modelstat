//! The capture surface: what a caller hands the SDK per LLM call, and the
//! (worker-side) conversion into wire records.
//!
//! Building an [`LlmCall`] and calling `Client::record` is the only thing that
//! happens on the live request path — it must stay a cheap move into a buffer.
//! All of the work here (redaction, hashing, id derivation) runs later, on the
//! background worker, off the hot path.

use crate::config::{Config, RedactionPolicy};
use crate::redact;
use crate::wire::{
    self, PricingMode, EventKind, GitContext, IngestBatch, RawEvent, TokenUsage, ToolCallStatus,
    ToolCallWire,
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// The excerpt cap for the standard (non-raw) path, in Unicode scalars.
const EXCERPT_MAX_CHARS: usize = 320;

/// One captured tool invocation. The SDK is in the call path, so it has the
/// real args and result — it hashes/sizes them here (never ships them raw).
#[derive(Debug, Clone)]
pub struct ToolCallInput {
    /// Bare tool name (`Bash`, `create_pr`).
    pub name: String,
    /// `builtin` or `mcp:<server>`.
    pub server: String,
    /// The call's arguments, if any. Hashed and sized; never shipped.
    pub args: Option<serde_json::Value>,
    /// Byte length of the result/output (the SDK sizes it; never ships it).
    pub result_bytes: u32,
    pub status: ToolCallStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// Allowlisted command verbs for shell-ish tools (≤3, each ≤40 chars).
    pub command_families: Vec<String>,
}

impl ToolCallInput {
    /// A minimal builtin tool call.
    #[must_use]
    pub fn new(name: impl Into<String>, status: ToolCallStatus) -> Self {
        Self {
            name: name.into(),
            server: "builtin".into(),
            args: None,
            result_bytes: 0,
            status,
            started_at: Utc::now(),
            ended_at: None,
            command_families: Vec::new(),
        }
    }
}

/// One captured LLM call. Construct with [`LlmCall::new`] and set the fields you
/// have; `prompt`/`completion` are raw here and are redacted on the worker.
#[derive(Debug, Clone)]
pub struct LlmCall {
    pub provider: String,
    pub model: Option<String>,
    /// Trace/conversation id used to group calls into a session downstream.
    pub session_id: String,
    pub kind: EventKind,
    pub tokens: TokenUsage,
    pub started_at: DateTime<Utc>,
    pub duration: Option<Duration>,
    pub prompt: Option<String>,
    pub completion: Option<String>,
    pub cwd: Option<String>,
    pub git: Option<GitContext>,
    pub pricing_mode: Option<PricingMode>,
    pub tool_calls: Vec<ToolCallInput>,
}

impl LlmCall {
    /// A call with `started_at = now` and `kind = assistant_message`.
    #[must_use]
    pub fn new(provider: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: None,
            session_id: session_id.into(),
            kind: EventKind::AssistantMessage,
            tokens: TokenUsage::default(),
            started_at: Utc::now(),
            duration: None,
            prompt: None,
            completion: None,
            cwd: None,
            git: None,
            pricing_mode: None,
            tool_calls: Vec::new(),
        }
    }

    /// Set the model.
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set token usage.
    #[must_use]
    pub fn tokens(mut self, tokens: TokenUsage) -> Self {
        self.tokens = tokens;
        self
    }

    /// Set the prompt and completion text (raw; redacted on the worker).
    #[must_use]
    pub fn text(mut self, prompt: impl Into<String>, completion: impl Into<String>) -> Self {
        self.prompt = Some(prompt.into());
        self.completion = Some(completion.into());
        self
    }
}

/// Truncate to at most `max` Unicode scalars, appending an elision marker.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

/// sha256 hex of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Build the privacy-reduced `(args_hash, signature_hash, args_bytes)` triple
/// for a tool call's arguments.
fn hash_args(args: Option<&serde_json::Value>) -> (String, String, u32) {
    let Some(args) = args else {
        return (String::new(), "none".into(), 0);
    };
    let serialized = serde_json::to_string(args).unwrap_or_default();
    let args_hash = sha256_hex(serialized.as_bytes());
    let signature = match args {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            sha256_hex(keys.join(",").as_bytes())
        }
        _ => "none".into(),
    };
    (args_hash, signature, serialized.len() as u32)
}

/// Convert one captured call into a wire event plus its tool-call records.
fn event_from_call(cfg: &Config, call: &LlmCall, seq: u64) -> (RawEvent, Vec<ToolCallWire>) {
    let source_ref = format!(
        "{}::{}::{}",
        call.session_id,
        call.started_at.timestamp_millis(),
        seq
    );
    let source_event_id = wire::source_event_id(&cfg.device_id, &source_ref);

    let content_excerpt = build_excerpt(cfg, call);

    let event = RawEvent {
        source_event_id: source_event_id.clone(),
        ts: call.started_at,
        kind: call.kind,
        agent: cfg.agent.clone(),
        provider: call.provider.clone(),
        model: call.model.clone(),
        session_id: call.session_id.clone(),
        tokens: call.tokens,
        cwd: call.cwd.clone(),
        git: call.git.clone(),
        duration_ms: call.duration.map(|d| d.as_millis() as u64),
        pricing_mode: call.pricing_mode,
        content_excerpt,
    };

    let tool_calls = call
        .tool_calls
        .iter()
        .enumerate()
        .map(|(i, tc)| {
            let (args_hash, signature_hash, args_bytes) = hash_args(tc.args.as_ref());
            let external_call_id = format!(
                "tc_{}",
                &wire::content_hash(&[&source_event_id, &i.to_string()])[..16]
            );
            ToolCallWire {
                external_call_id,
                session_id: call.session_id.clone(),
                source_event_id: source_event_id.clone(),
                segment_id: None,
                agent: cfg.agent.clone(),
                server: tc.server.clone(),
                name: tc.name.clone(),
                turn_index: None,
                call_index: i as u32,
                started_at: tc.started_at,
                ended_at: tc.ended_at,
                status: tc.status,
                args_hash,
                signature_hash,
                args_bytes,
                result_bytes: tc.result_bytes,
                model: call.model.clone(),
                command_families: tc.command_families.iter().take(3).cloned().collect(),
            }
        })
        .collect();

    (event, tool_calls)
}

/// Build the redacted excerpt from a call's prompt + completion, honoring the
/// configured redaction policy and (for the standard path) the 320-char cap.
fn build_excerpt(cfg: &Config, call: &LlmCall) -> Option<String> {
    let mut joined = String::new();
    if let Some(p) = &call.prompt {
        joined.push_str(p);
    }
    if let Some(c) = &call.completion {
        if !joined.is_empty() {
            joined.push_str("\n---\n");
        }
        joined.push_str(c);
    }
    if joined.is_empty() {
        return None;
    }

    let scrubbed = match cfg.redaction {
        RedactionPolicy::Floor => redact::redact(&joined).text,
        RedactionPolicy::None => joined,
    };

    // Raw mode ships the full (redacted) turns for server-side summarization;
    // the standard path caps the excerpt.
    if cfg.sends_full_turns() {
        Some(scrubbed)
    } else {
        Some(truncate_chars(&scrubbed, EXCERPT_MAX_CHARS))
    }
}

/// Drain a batch of captured calls into a wire [`IngestBatch`]. `seq` is a
/// monotonic counter used to keep per-call dedupe keys distinct within a run.
pub(crate) fn build_batch(
    cfg: &Config,
    calls: impl IntoIterator<Item = LlmCall>,
    seq: &mut u64,
) -> IngestBatch {
    let mut events = Vec::new();
    let mut tool_calls = Vec::new();
    let mut source_ids = Vec::new();

    for call in calls {
        *seq += 1;
        let (event, tcs) = event_from_call(cfg, &call, *seq);
        source_ids.push(event.source_event_id.clone());
        tool_calls.extend(tcs);
        events.push(event);
    }

    IngestBatch {
        batch_id: wire::batch_id(&source_ids),
        device_id: cfg.device_id.clone(),
        daemon_version: cfg.client_version.clone(),
        events,
        tool_calls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> Config {
        Config::new("msk_test", "raw_sdk_openai").with_device_id("dev_test")
    }

    #[test]
    fn build_batch_redacts_excerpt_and_caps_length() {
        let mut seq = 0;
        let call = LlmCall::new("openai", "sess_1").model("gpt-x").text(
            "here is my key sk-ant-0123456789abcdefghijABCDEF",
            "ok done",
        );
        let batch = build_batch(&cfg(), [call], &mut seq);

        assert_eq!(batch.events.len(), 1);
        let ev = &batch.events[0];
        assert_eq!(ev.agent, "raw_sdk_openai");
        assert_eq!(ev.provider, "openai");
        let excerpt = ev.content_excerpt.as_ref().unwrap();
        assert!(
            excerpt.contains("[REDACTED:anthropic_key]"),
            "got {excerpt:?}"
        );
        assert!(!excerpt.contains("sk-ant-0123"));
        assert!(excerpt.chars().count() <= EXCERPT_MAX_CHARS + 1);
        assert!(ev.source_event_id.starts_with("evt_"));
        assert!(batch.batch_id.starts_with("batch_"));
    }

    #[test]
    fn tool_calls_carry_hashes_not_raw_args() {
        let mut seq = 0;
        let mut call = LlmCall::new("anthropic", "sess_1");
        call.tool_calls.push(ToolCallInput {
            name: "Bash".into(),
            server: "builtin".into(),
            args: Some(serde_json::json!({ "command": "rm -rf /tmp/secret", "timeout": 5 })),
            result_bytes: 128,
            status: ToolCallStatus::Success,
            started_at: Utc::now(),
            ended_at: None,
            command_families: vec!["rm".into()],
        });
        let batch = build_batch(&cfg(), [call], &mut seq);

        assert_eq!(batch.tool_calls.len(), 1);
        let tc = &batch.tool_calls[0];
        assert_eq!(tc.name, "Bash");
        assert_eq!(
            tc.args_bytes,
            serde_json::to_string(
                &serde_json::json!({ "command": "rm -rf /tmp/secret", "timeout": 5 })
            )
            .unwrap()
            .len() as u32
        );
        assert_eq!(tc.args_hash.len(), 64); // sha256 hex
        assert_ne!(tc.signature_hash, "none");
        // The raw command must never appear anywhere in the serialized batch.
        let json = serde_json::to_string(&batch).unwrap();
        assert!(
            !json.contains("rm -rf /tmp/secret"),
            "raw args leaked into wire"
        );
    }

    #[test]
    fn raw_mode_sends_full_untruncated_turns_still_floor_redacted() {
        let cfg = Config::new("msk", "raw_sdk_openai")
            .with_device_id("dev_test")
            .with_remote("https://api.modelstat.ai", true);
        let mut seq = 0;
        let long = "word ".repeat(200); // > 320 chars
        let call = LlmCall::new("openai", "sess_1").text(long.clone(), "AKIAIOSFODNN7EXAMPLE");
        let batch = build_batch(&cfg, [call], &mut seq);
        let excerpt = batch.events[0].content_excerpt.as_ref().unwrap();
        assert!(
            excerpt.chars().count() > EXCERPT_MAX_CHARS,
            "raw mode must not truncate"
        );
        assert!(
            excerpt.contains("[REDACTED:aws_access_key]"),
            "floor still applies in raw mode"
        );
    }
}
