//! Muse (Meta's Muse Code CLI) session-log parser.
//!
//! Log home: `~/.local/share/muse/sessions/<Y>/<M>/<D>/<session-uuid>/` — one
//! directory per session, each holding a single append-only `session.jsonl`
//! (plus `tool-outputs/`, sqlite sidecars and logs the scan never reads).
//! The directory name IS the session id: it matches the `stream.id` every
//! record declares.
//!
//! Line shape: each line is either a session record
//! `{schema_version, id, stream, sequence, recorded_at, record_type,
//! durability, causation_id, payload_type, payload_schema_version, payload}`,
//! an `omitted_record` marker for live-only data the durable log never held,
//! or a `retained_frame` (a permission-transaction envelope whose children
//! are permission bookkeeping, never conversation — declined at the line).
//!
//! `recorded_at` is microseconds since the epoch. The parser renders it at
//! millisecond precision (`%.3fZ`), the exact shape codex stamps its own
//! lines with, so instants from every agent sort and compare as strings.
//!
//! What the durable log holds, per model step:
//!   * the typed prompt (`runtime.user_intent.accepted`, `run.started.prompt`)
//!   * the tool calls with their full JSON args
//!     (`run.assistant_tool_calls_committed`)
//!   * the tool results (`run.tool_result_batch_committed`, `task.output`)
//!   * the token counters (`run.model_completed.usage`, and the same numbers
//!     again as `run.goal_usage_attribution` — read once, from the former)
//!   * the model + provider ids, cwd, and session id
//!
//! What it deliberately does NOT hold: the assistant's prose. Text deltas are
//! live-only (`omitted_live_only`), so `assistant_message` events ship with
//! tokens but no excerpt — the codex parity, whose events likewise ship
//! excerpt-free. Anything downstream that needs prose reads the user turns
//! and the tool I/O, which are complete.
//!
//! Usage buckets, verified on real sessions: `cached_tokens` and
//! `cache_read_tokens` are the same number twice (identical on every observed
//! step — one of them is read, never both); `input_tokens` is INCLUSIVE of
//! the cached prefix (OpenAI semantics — the Meta API is OpenAI-compatible),
//! so the disjoint reading is `input − cached`; `reasoning_tokens` is always
//! ≤ `output_tokens`, i.e. inside it, so `output − reasoning`. The codex
//! double-billing fix applies unchanged: buckets that overlap on the wire
//! must not overlap in the event.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufReader;
use std::sync::OnceLock;

use modelstat_redact::redact;
use modelstat_wire::{source_event_id, tc_fallback_id, EventSource, RawEvent, TokenUsage};
use regex::Regex;
use serde_json::Value;

use crate::git::{guess_repo_slug_from_path, path_guessed_git_context};
use crate::line_reader::OffsetLines;
use crate::references::detect_event_references;
use crate::skips::{numeric_leaves, unknown_record_event, SkipLedger, UnknownRecord};
use crate::tool_action::{extract_local_tool_context, extract_tool_action, ToolActionInput};
use crate::tool_hash::{hash_args, json_bytes, split_observed_tool_name, tool_identity};
use crate::types::{LocalToolContext, ParseResult, ParseStats, ParserContext, Sink, ToolCallDraft};
use crate::util::{slice_utf16, stated_duration_ms};

/// The agent string this parser stamps: the tool the human used, following the
/// `claude_code` precedent (product "Muse Code", binary `muse`).
pub const AGENT_MUSE: &str = "muse_code";

/// The provider string Muse's own log states (`provider_id: "meta"`).
pub const PROVIDER_META: &str = "meta";

/// Muse session files live at
/// `…/sessions/<Y>/<M>/<D>/<session-uuid>/session.jsonl`: the parent directory
/// names the session. A path that does not parse this way names no session —
/// the records' own `stream.id` is the fallback, and a log that states
/// neither is unattributable.
pub fn derive_session_id_from_muse_path(path: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"sessions/[0-9]{4}/[0-9]{2}/[0-9]{2}/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})/session\.jsonl$",
        )
        .unwrap()
    });
    // The walk hands out native paths: on Windows the separators are `\`.
    // Flatten first so one expression names the session on every platform.
    let flat = path.replace('\\', "/");
    re.captures(&flat)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Microseconds since the epoch → the ISO-8601 UTC instant every other event
/// in every other parser's output carries, to the millisecond. `None` when
/// the number is out of range, so the caller withholds the event's position
/// rather than inventing one.
fn iso_from_epoch_micros(micros: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_micros(micros)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// The record's own instant: `recorded_at` microseconds. Missing or
/// non-numeric states nothing — the caller skips the record, never the clock.
fn record_ts(obj: &Value) -> Option<String> {
    let micros = obj
        .get("recorded_at")
        .and_then(Value::as_u64)
        .map(|m| m as i64)
        .or_else(|| obj.get("recorded_at").and_then(Value::as_i64))?;
    iso_from_epoch_micros(micros)
}

/// The provider string the log states, lowercased, or `"unknown"` when it
/// states none — the pi rule: the wire's `provider` is required, so silence
/// needs a word, and that word is not a claim about any vendor.
fn provider_of(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .unwrap_or_else(|| "unknown".to_string())
}

/// Join every `text` block's text (assets/attachments dropped — only the words
/// ride the excerpt; binary blobs never do).
fn join_text_blocks(blocks: &Value) -> String {
    match blocks {
        Value::Array(items) => items
            .iter()
            .filter(|b| b.get("kind").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<&str>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// SPEC 0005: redacted VERBATIM text + its length in chars (mirrors
/// `pi::extract_excerpt` — nothing processed away, no truncation; the wire
/// clamp is the only, extreme, bound).
fn extract_excerpt(text: &str) -> Option<(String, u64)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let pre_chars = text.chars().count() as u64;
    let cleaned = redact(text, None).text;
    if cleaned.is_empty() {
        None
    } else {
        Some((cleaned, pre_chars))
    }
}

fn collect_ref_text(text: &str) -> String {
    slice_utf16(text, 64_000)
}

/// One model step's token counters, or an honest statement that they could
/// not be read.
///
/// All six counters must be present and numeric, or the whole reading is
/// withheld and every number `usage` did state rides `tokens_unmapped`
/// instead (the pi rule — a partial bucket is a claim about the missing
/// piece, and the wire's five buckets always materialise).
///
/// The disjoint mapping, verified on real sessions (see the module docs):
/// `cached_tokens` and `cache_read_tokens` are one number stated twice;
/// `input_tokens` holds the cached prefix inside it; `reasoning_tokens`
/// sits inside `output_tokens`.
fn muse_token_usage(usage: &Value) -> (Option<TokenUsage>, BTreeMap<String, u64>) {
    if usage.is_null() {
        return (None, BTreeMap::new());
    }
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    let (
        Some(input_tokens),
        Some(output_tokens),
        Some(cached_tokens),
        Some(cache_write_tokens),
        Some(reasoning_tokens),
    ) = (
        field("input_tokens"),
        field("output_tokens"),
        field("cached_tokens"),
        field("cache_write_tokens"),
        field("reasoning_tokens"),
    )
    else {
        return (None, numeric_leaves(usage));
    };
    // `cache_read_tokens` is deliberately unread: it duplicates `cached_tokens`
    // on every observed step, and reading both would double-bill the cache.
    (
        Some(TokenUsage {
            input: input_tokens.saturating_sub(cached_tokens),
            output: output_tokens.saturating_sub(reasoning_tokens),
            cache_creation: cache_write_tokens,
            cache_read: cached_tokens,
            reasoning: reasoning_tokens,
        }),
        BTreeMap::new(),
    )
}

/// A `retained_frame` envelope holds permission bookkeeping (the observed
/// frame name is `session_permission_transaction`), never conversation — so
/// it is declined at the line, before any child mints an id. Any OTHER frame
/// name is a shape nobody has seen: unwrap it, so a future frame that carries
/// conversation cannot pass silently.
fn is_permission_frame(obj: &Value) -> bool {
    obj.get("retained_frame").and_then(Value::as_str) == Some("session_permission_transaction")
}

/// Payload types this parser models and declines: execution bookkeeping whose
/// facts already ride another record. Declined on purpose, so never counted —
/// a decision, not a failure (the codex `response_item/message` precedent).
fn is_declined_payload(payload_type: &str, kind: &str, event_kind: Option<&str>) -> bool {
    match payload_type {
        // Session bookkeeping: identity, timing, naming, approvals, intake.
        "session.opened.observed"
        | "session.startup_phases.observed"
        | "session.name.changed"
        | "runtime.command_intake.session_name.received"
        | "runtime.command_intake.settled"
        | "runtime.retained_fact"
        | "runtime.user_intent.materialized"
        | "runtime.session.task"
        | "command.invoked" => true,
        "runtime.session" => matches!(
            (kind, event_kind),
            // Model + provider declarations (state, harvested above).
            ("run_model", _)
                | ("retained_runtime_fact", _)
                | ("route_facts", _)
                | ("metadata", _)
                // Run/task lifecycle: linking, options, traces, diagnostics.
                | ("run", Some("started"))
                | ("run", Some("model_request_configured"))
                | ("run", Some("provider_request_options_configured"))
                | ("run", Some("model_input_trace_recorded"))
                | ("run", Some("model_response_created"))
                | ("run", Some("task_stream_linked"))
                | ("run", Some("reasoning_committed"))
                | ("run", Some("reasoning_summary_delta"))
                | ("run", Some("reasoning_summary_committed"))
                | ("run", Some("context_block_diagnostic"))
                | ("run", Some("resource_usage_sampled"))
                // The step's counters, restated: `run.model_completed`
                // already carried them (same numbers, same step), so reading
                // this rollup too would double every step's spend.
                | ("run", Some("goal_usage_attribution"))
                // Runtime bookkeeping, not conversation: the agent-tree
                // capacity declaration, todo-list snapshots (prompt-derived
                // text the user turns already capture), background-task inbox
                // delivery (the underlying tool result is captured via the
                // result batch), and the approval policy audit trail (a
                // denied call never commits, so there is nothing to pair).
                | ("agent_tree_initialized", _)
                | ("run", Some("todo_snapshot_updated"))
                | ("run", Some("inbox_item_queued"))
                | ("run", Some("inbox_item_drained"))
                | ("run", Some("inbox_delivery_anomaly"))
                | ("approval", _)
                // Tool lifecycle: declaration (the commit carries the args) and
                // per-task output chunks (the result batch carries the text).
                | ("task", _)
        ),
        // One effect's start: the commit carries the call, the terminal
        // carries its end. The start states neither.
        "tool_batch.effect.started" => true,
        _ => false,
    }
}

pub fn parse_muse_session(ctx: &ParserContext) -> std::io::Result<ParseResult> {
    let mut sink = Sink::collect();
    let (tool_calls, script_contexts, stats, skips) = parse_inner(ctx, &mut sink)?;
    sink.flush();
    Ok(ParseResult {
        events: sink.take_collected(),
        tool_calls,
        script_contexts,
        stats,
        skipped_kinds: skips.into_counts(),
        session_actors: Default::default(),
        source_file: ctx.source_file.clone(),
    })
}

pub fn parse_muse_session_streaming(
    ctx: &ParserContext,
    emit: &mut dyn FnMut(Vec<RawEvent>),
) -> std::io::Result<ParseResult> {
    let mut sink = Sink::stream(emit);
    let (tool_calls, script_contexts, stats, skips) = parse_inner(ctx, &mut sink)?;
    sink.flush();
    Ok(ParseResult {
        events: Vec::new(),
        tool_calls,
        script_contexts,
        stats,
        skipped_kinds: skips.into_counts(),
        session_actors: Default::default(),
        source_file: ctx.source_file.clone(),
    })
}

fn parse_inner(
    ctx: &ParserContext,
    sink: &mut Sink,
) -> std::io::Result<(
    Vec<ToolCallDraft>,
    Vec<LocalToolContext>,
    ParseStats,
    SkipLedger,
)> {
    let mut tool_calls: Vec<ToolCallDraft> = Vec::new();
    let mut script_contexts: Vec<LocalToolContext> = Vec::new();
    let mut pending_by_call_id: HashMap<String, usize> = HashMap::new();

    let mut raw_lines: u64 = 0;
    let mut emitted: u64 = 0;
    let mut skipped: u64 = 0;
    let mut skips = SkipLedger::default();

    let file = File::open(&ctx.source_file)?;
    let mut lines = OffsetLines::new(BufReader::new(file), ctx.byte_offset_start);

    let mut session_id: Option<String> = derive_session_id_from_muse_path(&ctx.source_file);
    let mut cwd: Option<String> = None;
    let mut last_provider: Option<String> = None;
    let mut last_model: Option<String> = None;
    // Conversation turn ordinal (SPEC 0005) — see the pi parser.
    let mut current_turn: u64 = 0;
    let mut saw_user_prompt = false;
    // The open model step's tool footprint: aggregate counts plus the arg and
    // result text its references are mined from. Attached to the step's own
    // usage-bearing event, then cleared — the codex pending-aggregate rule.
    let mut step_aggregate: BTreeMap<String, u64> = BTreeMap::new();
    // Every path this step's calls named — Muse is routinely launched from a
    // directory that HOLDS repos rather than being one, and then its `cwd`
    // states no repo at all. These are the only facts that can (the pi rule).
    let mut step_tool_paths: Vec<String> = Vec::new();
    let mut step_ref_text = String::new();

    let agent = ctx.agent(AGENT_MUSE);

    while let Some((line, offset)) = lines.next_line()? {
        raw_lines += 1;
        if line.trim().is_empty() {
            skipped += 1;
            continue;
        }
        let obj: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        // Permission-transaction envelopes: bookkeeping, never conversation.
        if obj.get("retained_frame").is_some() {
            if !is_permission_frame(&obj) {
                skips.drop_record(&ctx.source_file, "retained_frame/unexpected");
            }
            skipped += 1;
            continue;
        }

        // Live-only markers: the durable log never held the data, so there is
        // nothing to model — but the dialect is counted, so a new omission
        // class cannot pass silently.
        if let Some(omitted) = obj.get("omitted_record") {
            let class = omitted
                .get("omission_class")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            skips.drop_record(&ctx.source_file, &format!("omitted/{class}"));
            skipped += 1;
            continue;
        }

        let Some(payload_type) = obj.get("payload_type").and_then(Value::as_str) else {
            skips.drop_record(&ctx.source_file, "record-without-payload_type");
            skipped += 1;
            continue;
        };
        let payload = obj.get("payload").cloned().unwrap_or(Value::Null);
        let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
        let event_kind = payload
            .get("event")
            .and_then(|e| e.get("kind"))
            .and_then(Value::as_str);

        // The path names nobody: take the log's own word for the session, once.
        if session_id.is_none() {
            if let Some(sid) = obj
                .get("stream")
                .and_then(|s| s.get("id"))
                .and_then(Value::as_str)
            {
                session_id = Some(sid.to_string());
            }
        }

        // State declarations: provider, model, cwd — harvested, never emitted.
        // These ride their OWN payload types (`runtime.session.metadata`,
        // not `runtime.session` — verified on a live log, where the latter
        // shape never appears).
        if payload_type == "runtime.session.metadata" && kind == "metadata" {
            if let Some(record) = payload.get("record") {
                if let Some(p) = record.get("provider_id").and_then(Value::as_str) {
                    last_provider = Some(p.to_string());
                }
                if let Some(m) = record.get("model_id").and_then(Value::as_str) {
                    last_model = Some(m.to_string());
                }
                if cwd.is_none() {
                    if let Some(w) = record.get("workspace_root").and_then(Value::as_str) {
                        cwd = Some(w.to_string());
                    }
                }
            }
            continue;
        }
        if payload_type == "runtime.session.route_facts" && kind == "route_facts" {
            if cwd.is_none() {
                if let Some(c) = payload
                    .get("record")
                    .and_then(|r| r.get("cwd"))
                    .and_then(Value::as_str)
                {
                    cwd = Some(c.to_string());
                }
            }
            continue;
        }
        if payload_type == "run.model.configured" {
            if let Some(p) = payload
                .get("record")
                .and_then(|r| r.get("provider_id").and_then(Value::as_str))
            {
                last_provider = Some(p.to_string());
            }
            if let Some(m) = payload
                .get("record")
                .and_then(|r| r.get("model_id").and_then(Value::as_str))
            {
                last_model = Some(m.to_string());
            }
            continue;
        }

        if is_declined_payload(payload_type, kind, event_kind) {
            continue;
        }

        // A record this parser models needs its session and its instant; a
        // modelled kind without either is the same silent failure as an
        // unknown kind (the pi rule).
        let (Some(sid), Some(ts)) = (session_id.clone(), record_ts(&obj)) else {
            skipped += 1;
            skips.drop_record(&ctx.source_file, payload_type);
            continue;
        };

        // The typed prompt.
        if payload_type == "runtime.user_intent.accepted" {
            let text = {
                let refill = payload
                    .get("refill_blocks")
                    .map(join_text_blocks)
                    .unwrap_or_default();
                if refill.trim().is_empty() {
                    // Older shapes may carry only the model messages.
                    payload
                        .get("model_messages")
                        .and_then(Value::as_array)
                        .map(|msgs| {
                            msgs.iter()
                                .filter_map(|m| m.get("content"))
                                .map(join_text_blocks)
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default()
                } else {
                    refill
                }
            };
            let (excerpt, content_bytes) = match extract_excerpt(&text) {
                Some((t, c)) => (Some(t), Some(c)),
                None => (None, None),
            };
            // A real (typed) prompt starts a new turn — SPEC 0005, mirroring
            // the pi parser.
            if excerpt.is_some() {
                if saw_user_prompt {
                    current_turn += 1;
                }
                saw_user_prompt = true;
            }
            let refs = detect_event_references(&collect_ref_text(&text));
            let slug = guess_repo_slug_from_path(cwd.as_deref());
            sink.push(RawEvent {
                seq: Some(raw_lines),
                started_at: None,
                first_token_at: None,
                source_event_id: source_event_id(
                    &ctx.device_id,
                    &EventSource::File {
                        file: &ctx.source_file,
                        byte_offset: offset,
                    },
                ),
                ts,
                kind: "user_message".to_string(),
                agent: agent.clone(),
                provider: provider_of(last_provider.as_deref()),
                model: last_model.clone(),
                session_id: sid,
                actor_id: None,
                recipient_actor_id: None,
                turn_index: Some(current_turn),
                parent_event_id: None,
                cwd: cwd.clone(),
                git: path_guessed_git_context(slug, None),
                tokens: None,
                tokens_unmapped: BTreeMap::new(),
                duration_ms: None,
                tool_calls: BTreeMap::new(),
                tool_paths: Vec::new(),
                files_touched: Vec::new(),
                content_excerpt: excerpt,
                content_bytes,
                reasoning_excerpt: None,
                reasoning_bytes: None,
                references: refs,
                source_file: Some(ctx.source_file.clone()),
                source_byte_offset: Some(offset),
                redactions: Default::default(),
            });
            emitted += 1;
            continue;
        }

        // One model step's tool calls, with their full JSON args.
        if payload_type == "runtime.session"
            && kind == "run"
            && event_kind == Some("assistant_tool_calls_committed")
        {
            let event = payload.get("event").cloned().unwrap_or(Value::Null);
            let event_id = source_event_id(
                &ctx.device_id,
                &EventSource::File {
                    file: &ctx.source_file,
                    byte_offset: offset,
                },
            );
            let calls = event
                .get("tool_calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (index, call) in calls.iter().enumerate() {
                let observed = call
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                if observed.is_empty() {
                    continue;
                }
                let (server, name) = split_observed_tool_name(observed);
                // `args` is a JSON string on the wire; parsed so the hash and
                // the action read the object, never the whitespace.
                let input: Value = call
                    .get("args")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(Value::Null);
                let hashes = hash_args(&input);
                RawEvent::collect_tool_paths(&input, &mut step_tool_paths);
                let call_id = call.get("call_id").and_then(Value::as_str);
                // `call_id` is the join key every later record references; it
                // rides the draft verbatim (a normalised copy would fuse two
                // calls whose ids differ only in a hex tail).
                let external_call_id = match call_id {
                    Some(s) if !s.trim().is_empty() => slice_utf16(s.trim(), 120),
                    _ => tc_fallback_id(&event_id, index as u64),
                };
                if let Some((command, ctx_cwd)) = extract_local_tool_context(&ToolActionInput {
                    server: &server,
                    name: &name,
                    input: &input,
                    cwd: cwd.as_deref(),
                }) {
                    script_contexts.push(LocalToolContext {
                        external_call_id: external_call_id.clone(),
                        command,
                        cwd: ctx_cwd,
                    });
                }
                let action = extract_tool_action(&ToolActionInput {
                    server: &server,
                    name: &name,
                    input: &input,
                    cwd: cwd.as_deref(),
                });
                let draft = ToolCallDraft {
                    external_call_id: external_call_id.clone(),
                    session_id: sid.clone(),
                    source_event_id: event_id.clone(),
                    agent: agent.clone(),
                    server: server.clone(),
                    name: name.clone(),
                    turn_index: Some(current_turn),
                    call_index: index as u64,
                    started_at: ts.clone(),
                    ended_at: None,
                    status: "unknown".to_string(),
                    args_hash: hashes.args_hash,
                    signature_hash: hashes.signature_hash,
                    args_bytes: hashes.args_bytes,
                    result_bytes: 0,
                    model: last_model.clone(),
                    action: Some(action),
                };
                let identity = tool_identity(&server, &name);
                *step_aggregate.entry(identity).or_insert(0) += 1;
                // The args text is turn content for the reference miner: a
                // shell command names the repos and files the step touched.
                step_ref_text
                    .push_str(call.get("args").and_then(Value::as_str).unwrap_or_default());
                step_ref_text.push('\n');
                tool_calls.push(draft);
                if let Some(id) = call_id {
                    if !id.trim().is_empty() {
                        pending_by_call_id.insert(id.to_string(), tool_calls.len() - 1);
                    }
                }
            }
            continue;
        }

        // One model step's results, paired onto their drafts by `call_id`.
        if payload_type == "runtime.session"
            && kind == "run"
            && event_kind == Some("tool_result_batch_committed")
        {
            let event = payload.get("event").cloned().unwrap_or(Value::Null);
            let results = event
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for result in &results {
                let Some(call_id) = result.get("tool_call_id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(idx) = pending_by_call_id.get(call_id).copied() else {
                    continue;
                };
                let text = result.get("text").and_then(Value::as_str).unwrap_or("");
                let draft = &mut tool_calls[idx];
                // The terminal owns the end instant (it carries the status);
                // the batch fills it only when no terminal ever arrives, so a
                // call the log never saw finish keeps no fabricated end.
                if draft.ended_at.is_none() {
                    draft.ended_at = Some(ts.clone());
                }
                draft.result_bytes = json_bytes(&Value::String(text.to_string()));
                step_ref_text.push_str(text);
                step_ref_text.push('\n');
            }
            continue;
        }

        // One tool call's end: status + end instant, by `call_id`.
        if payload_type == "tool_batch.effect.terminal" {
            let record = payload.get("record").cloned().unwrap_or(Value::Null);
            let Some(call_id) = record.get("call_id").and_then(Value::as_str) else {
                skipped += 1;
                skips.drop_record(&ctx.source_file, payload_type);
                continue;
            };
            let outcome = record
                .get("outcome")
                .and_then(|o| o.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("");
            // `completed` is success, `failed` is error; anything else
            // (cancelled, …) keeps the honest `unknown` — a call nobody saw
            // finish is unknown, not zero and not infinite.
            let status = match outcome {
                "completed" => "success",
                "failed" => "error",
                _ => "unknown",
            };
            // Looked up, never removed: the result batch pairs by the same key
            // and may arrive after the terminal.
            if let Some(idx) = pending_by_call_id.get(call_id).copied() {
                let draft = &mut tool_calls[idx];
                draft.ended_at = Some(ts.clone());
                if status != "unknown" {
                    draft.status = status.to_string();
                }
            }
            continue;
        }

        // One model step's counters: the usage-bearing event, carrying the
        // step's tool footprint. The assistant's prose is live-only, so this
        // event ships excerpt-free (the codex parity, documented above).
        if payload_type == "runtime.session"
            && kind == "run"
            && event_kind == Some("model_completed")
        {
            let event = payload.get("event").cloned().unwrap_or(Value::Null);
            let usage = event.get("usage").cloned().unwrap_or(Value::Null);
            let (tokens, tokens_unmapped) = muse_token_usage(&usage);
            if tokens.is_none() && !tokens_unmapped.is_empty() {
                skips.drop_record(&ctx.source_file, "run/model_completed/usage");
            }
            let model = event
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| last_model.clone());
            if let Some(m) = model.clone() {
                last_model = Some(m);
            }
            let refs = detect_event_references(&collect_ref_text(&step_ref_text));
            let slug = guess_repo_slug_from_path(cwd.as_deref());
            let git = path_guessed_git_context(slug.clone(), None);
            sink.push(RawEvent {
                seq: Some(raw_lines),
                started_at: None,
                first_token_at: None,
                source_event_id: source_event_id(
                    &ctx.device_id,
                    &EventSource::File {
                        file: &ctx.source_file,
                        byte_offset: offset,
                    },
                ),
                ts,
                kind: "assistant_message".to_string(),
                agent: agent.clone(),
                provider: provider_of(last_provider.as_deref()),
                model,
                session_id: sid,
                actor_id: None,
                recipient_actor_id: None,
                turn_index: Some(current_turn),
                parent_event_id: None,
                cwd: cwd.clone(),
                git,
                tokens,
                tokens_unmapped,
                duration_ms: stated_duration_ms(&event),
                tool_calls: std::mem::take(&mut step_aggregate),
                tool_paths: std::mem::take(&mut step_tool_paths),
                files_touched: Vec::new(),
                content_excerpt: None,
                content_bytes: None,
                reasoning_excerpt: None,
                reasoning_bytes: None,
                references: refs,
                source_file: Some(ctx.source_file.clone()),
                source_byte_offset: Some(offset),
                redactions: Default::default(),
            });
            step_ref_text.clear();
            emitted += 1;
            continue;
        }

        // A payload type nothing here models — the record's own word, verbatim.
        skips.drop_record(&ctx.source_file, payload_type);
        sink.push(unknown_record_event(UnknownRecord {
            seq: Some(raw_lines),
            kind: payload_type,
            source_event_id: source_event_id(
                &ctx.device_id,
                &EventSource::File {
                    file: &ctx.source_file,
                    byte_offset: offset,
                },
            ),
            agent: &agent,
            provider: &provider_of(last_provider.as_deref()),
            session_id: sid,
            ts,
            turn_index: Some(current_turn),
            duration_ms: stated_duration_ms(&obj),
            source_file: &ctx.source_file,
            source_byte_offset: Some(offset),
        }));
        emitted += 1;
    }

    Ok((
        tool_calls,
        script_contexts,
        ParseStats {
            raw_lines,
            emitted_events: emitted,
            skipped,
        },
        skips,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_comes_from_the_date_partitioned_path() {
        assert_eq!(
            derive_session_id_from_muse_path(
                "/Users/dev/.local/share/muse/sessions/2026/09/07/01a07b86-0612-7f61-ba7b-46aded41c21e/session.jsonl"
            ),
            Some("01a07b86-0612-7f61-ba7b-46aded41c21e".to_string())
        );
        // Windows separators name the session too.
        assert_eq!(
            derive_session_id_from_muse_path(
                "C:\\Users\\dev\\.local\\share\\muse\\sessions\\2026\\09\\07\\01a07b86-0612-7f61-ba7b-46aded41c21e\\session.jsonl"
            ),
            Some("01a07b86-0612-7f61-ba7b-46aded41c21e".to_string())
        );
        // Not a muse session path: no session is named.
        assert_eq!(
            derive_session_id_from_muse_path(
                "/Users/dev/.codex/sessions/2026/09/07/rollout-x.jsonl"
            ),
            None
        );
        assert_eq!(
            derive_session_id_from_muse_path("/tmp/tool-outputs/call_1-bash.txt"),
            None
        );
    }

    #[test]
    fn micros_render_at_millisecond_precision() {
        // A real `recorded_at` from a live session.
        assert_eq!(
            iso_from_epoch_micros(1_788_778_776_104_883),
            Some("2026-09-07T10:59:36.104Z".to_string())
        );
        assert_eq!(iso_from_epoch_micros(i64::MAX), None);
    }

    #[test]
    fn usage_buckets_are_disjoint() {
        // `cached_tokens` == `cache_read_tokens` on the wire (one number
        // twice); `input_tokens` holds the cached prefix inside it, and
        // `reasoning_tokens` sits inside `output_tokens`. All three overlaps
        // verified on real sessions — see the module docs.
        let usage = serde_json::json!({
            "input_tokens": 31351,
            "output_tokens": 132,
            "cached_tokens": 30065,
            "cache_write_tokens": 0,
            "cache_read_tokens": 30065,
            "reasoning_tokens": 28
        });
        let (tokens, unmapped) = muse_token_usage(&usage);
        assert!(unmapped.is_empty());
        let t = tokens.expect("complete counters map");
        assert_eq!(t.input, 1286);
        assert_eq!(t.output, 104);
        assert_eq!(t.cache_read, 30065);
        assert_eq!(t.cache_creation, 0);
        assert_eq!(t.reasoning, 28);
    }

    #[test]
    fn partial_counters_withhold_the_whole_reading() {
        let usage = serde_json::json!({"input_tokens": 10, "output_tokens": 5});
        let (tokens, unmapped) = muse_token_usage(&usage);
        assert!(tokens.is_none());
        assert_eq!(unmapped.get("input_tokens"), Some(&10));
    }
}
