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
    self, cap_metadata, EventKind, GitContext, IngestBatch, RawEvent, TokenUsage, ToolCallStatus,
    ToolCallWire,
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
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
    /// When the first piece of the model's output arrived, when the caller
    /// watched for it (a streamed response). Left `None` on a call that returns
    /// in one piece — there is no first chunk to have seen.
    pub first_token_at: Option<DateTime<Utc>>,
    pub duration: Option<Duration>,
    pub prompt: Option<String>,
    pub completion: Option<String>,
    pub cwd: Option<String>,
    pub git: Option<GitContext>,
    pub tool_calls: Vec<ToolCallInput>,
    /// Per-call attribution tags. Highest-priority layer: these override
    /// `Config::metadata` for any shared key. Capped before send.
    pub metadata: BTreeMap<String, String>,
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
            first_token_at: None,
            duration: None,
            prompt: None,
            completion: None,
            cwd: None,
            git: None,
            // Unknown until the caller says otherwise: an SDK user who never
            // touches this field has not told us how they pay, and guessing is
            // what put $39.65 of imaginary spend on a subscription account.
            tool_calls: Vec::new(),
            metadata: BTreeMap::new(),
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

    /// State when the first piece of the model's output arrived. Call this from
    /// a streaming loop, on the first chunk; leave it unset otherwise.
    #[must_use]
    pub fn first_token_at(mut self, at: DateTime<Utc>) -> Self {
        self.first_token_at = Some(at);
        self
    }

    /// Set the prompt and completion text (raw; redacted on the worker).
    #[must_use]
    pub fn text(mut self, prompt: impl Into<String>, completion: impl Into<String>) -> Self {
        self.prompt = Some(prompt.into());
        self.completion = Some(completion.into());
        self
    }

    /// Attach a single per-call attribution tag (overwriting any previous value
    /// for `key`). Per-call tags override [`Config::metadata`] for shared keys.
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Merge a map of per-call attribution tags (each overwriting any previous
    /// value for the same key).
    #[must_use]
    pub fn with_metadata<K, V>(mut self, tags: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in tags {
            self.metadata.insert(k.into(), v.into());
        }
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
    let metadata = resolve_metadata(cfg, call);

    let event = RawEvent {
        source_event_id: source_event_id.clone(),
        ts: call.started_at,
        // The SDK held the clock for this call, so it states the span's ends
        // rather than leaving a reader to reconstruct them from `ts` and a
        // duration that may not be set.
        started_at: Some(call.started_at),
        first_token_at: call.first_token_at,
        kind: call.kind,
        agent: cfg.agent.clone(),
        provider: call.provider.clone(),
        model: call.model.clone(),
        session_id: call.session_id.clone(),
        tokens: call.tokens,
        cwd: call.cwd.clone(),
        git: call.git.clone(),
        duration_ms: call.duration.map(|d| d.as_millis() as u64),
        content_excerpt,
        metadata,
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

/// Resolve the per-event metadata: `Config` defaults are the base layer and
/// per-call tags override them (per-call wins on a shared key), then the caps
/// are applied. Returns `None` when the merged map is empty so the wire key is
/// omitted. (The Rust SDK has no ambient layer — see the crate README.)
fn resolve_metadata(cfg: &Config, call: &LlmCall) -> Option<BTreeMap<String, String>> {
    if cfg.metadata.is_empty() && call.metadata.is_empty() {
        return None;
    }
    let mut merged = cfg.metadata.clone();
    // Per-call wins: inserting overwrites any default with the same key.
    for (k, v) in &call.metadata {
        merged.insert(k.clone(), v.clone());
    }
    let capped = cap_metadata(merged);
    if capped.is_empty() {
        None
    } else {
        Some(capped)
    }
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
    // Each session named to its account — the app — under the provider its
    // events actually used, so attribution happens AT INGEST, per app, like a
    // daemon-observed login. A session's calls share one provider in practice;
    // if they ever didn't, last-write-wins matches the map semantics.
    let mut session_installs = std::collections::BTreeMap::new();

    for call in calls {
        *seq += 1;
        if !call.provider.is_empty() {
            session_installs.insert(
                call.session_id.clone(),
                wire::SessionInstall {
                    provider_account_id: cfg.app.clone(),
                    provider: call.provider.clone(),
                },
            );
        }
        let (event, tcs) = event_from_call(cfg, &call, *seq);
        source_ids.push(event.source_event_id.clone());
        tool_calls.extend(tcs);
        events.push(event);
    }

    IngestBatch {
        batch_id: wire::batch_id(&source_ids),
        device_id: cfg.device_id.clone(),
        daemon_version: cfg.client_version.clone(),
        app: Some(cfg.app.clone()),
        events,
        tool_calls,
        session_installs,
        // Always send an explicit value reflecting the config so backend usage
        // is off-by-default but users can opt in.
        auto_taxonomy: Some(cfg.auto_taxonomy),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg() -> Config {
        Config::new("msk_test", "raw_sdk_openai", "test-app").with_device_id("dev_test")
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

    /// The SDK is in the call path, so it can state the span's ends. `started_at`
    /// always ships (it is `ts`'s own provenance made explicit); the first-token
    /// instant ships only when the caller watched a stream and said so, because a
    /// call that returns in one piece never had a first chunk to time.
    #[test]
    fn the_call_states_the_instants_it_saw_and_omits_the_one_it_did_not() {
        let mut seq = 0;
        let quiet = LlmCall::new("openai", "sess_1");
        let started = quiet.started_at;
        let streamed =
            LlmCall::new("openai", "sess_2").first_token_at(started + Duration::from_millis(140));

        let batch = build_batch(&cfg(), [quiet, streamed], &mut seq);
        let (a, b) = (&batch.events[0], &batch.events[1]);

        assert_eq!(a.started_at, Some(started));
        assert_eq!(
            a.ts, started,
            "ts is unchanged — the new field sits beside it"
        );
        assert_eq!(a.first_token_at, None, "no stream, no first chunk to time");
        assert_eq!(
            b.first_token_at,
            Some(started + Duration::from_millis(140)),
            "the instant the caller stated survives verbatim"
        );

        // Additive on the wire: absent means absent, never null.
        let json = serde_json::to_value(a).unwrap();
        assert!(json.get("first_token_at").is_none());
        assert!(json.get("started_at").is_some());
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
    fn auto_taxonomy_defaults_off_and_opts_in() {
        // Default config: taxonomy off → explicit `auto_taxonomy: false`.
        let mut seq = 0;
        let batch = build_batch(&cfg(), [LlmCall::new("openai", "sess_1")], &mut seq);
        assert_eq!(batch.auto_taxonomy, Some(false));
        let j: serde_json::Value = serde_json::to_value(&batch).unwrap();
        assert_eq!(j["auto_taxonomy"], false);

        // Opt in: flag true → wire `auto_taxonomy: true`.
        let mut on = cfg();
        on.auto_taxonomy = true;
        let mut seq = 0;
        let batch = build_batch(&on, [LlmCall::new("openai", "sess_1")], &mut seq);
        assert_eq!(batch.auto_taxonomy, Some(true));
        let j: serde_json::Value = serde_json::to_value(&batch).unwrap();
        assert_eq!(j["auto_taxonomy"], true);
    }

    #[test]
    fn metadata_merges_with_per_call_winning_and_omits_when_empty() {
        // No metadata anywhere → key omitted on the wire.
        let mut seq = 0;
        let batch = build_batch(&cfg(), [LlmCall::new("openai", "sess_1")], &mut seq);
        assert!(batch.events[0].metadata.is_none());
        let j: serde_json::Value = serde_json::to_value(&batch).unwrap();
        assert!(j["events"][0].get("metadata").is_none());

        // Config defaults < per-call: per-call `feature` overrides the default,
        // the default-only `environment` survives, the per-call-only `team` is
        // added.
        let cfg = cfg()
            .with_metadata("environment", "prod")
            .with_metadata("feature", "default_feature");
        let call = LlmCall::new("openai", "sess_1")
            .metadata("feature", "search")
            .with_metadata([("team", "growth")]);
        let mut seq = 0;
        let batch = build_batch(&cfg, [call], &mut seq);
        let md = batch.events[0].metadata.as_ref().unwrap();
        assert_eq!(md.get("environment").map(String::as_str), Some("prod"));
        assert_eq!(md.get("feature").map(String::as_str), Some("search"));
        assert_eq!(md.get("team").map(String::as_str), Some("growth"));
        // It serializes as a flat object.
        let j: serde_json::Value = serde_json::to_value(&batch).unwrap();
        assert_eq!(j["events"][0]["metadata"]["feature"], "search");
    }

    #[test]
    fn metadata_caps_excess_keys_by_sorted_order() {
        // 20 default keys "k00".."k19"; only the 16 smallest survive (k00..k15).
        let mut over = cfg();
        for i in 0..20 {
            over = over.with_metadata(format!("k{i:02}"), "v");
        }
        let mut seq = 0;
        let batch = build_batch(&over, [LlmCall::new("openai", "sess_1")], &mut seq);
        let md = batch.events[0].metadata.as_ref().unwrap();
        assert_eq!(md.len(), 16);
        assert!(md.contains_key("k15"));
        assert!(!md.contains_key("k16"));
    }

    #[test]
    fn metadata_truncates_over_long_values_before_send() {
        let call = LlmCall::new("openai", "sess_1").metadata("big", "v".repeat(500));
        let mut seq = 0;
        let batch = build_batch(&cfg(), [call], &mut seq);
        let md = batch.events[0].metadata.as_ref().unwrap();
        assert_eq!(md.get("big").unwrap().chars().count(), 256);
    }

    #[test]
    fn raw_mode_sends_full_untruncated_turns_still_floor_redacted() {
        let cfg = Config::new("msk", "raw_sdk_openai", "test-app")
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
