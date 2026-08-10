//! Codex CLI rollout parser — a byte-for-byte port of
//! `packages/parsers/src/codex/index.ts`.
//!
//! Tool calls come from `response_item` payloads and become drafts (never
//! events); the aggregate identity→count map attaches to the next emitted
//! assistant event. Token accounting stores DISJOINT buckets (input excl. cache,
//! output excl. reasoning) — the double-billing fix (feature §7.1).
//!
//! Token counters live at `payload.info.last_token_usage` — see
//! [`codex_last_token_usage`] for why that exact path, and why a counter that
//! has moved is reported rather than either zeroed or thrown.
//!
//! PARITY: the TS event_msg path falls back to `new Date().toISOString()` when a
//! line has no timestamp. That is non-deterministic and non-replayable, so the
//! Rust port falls back to the last-seen line timestamp instead (deterministic);
//! codex always writes timestamps, so this never differs in practice.

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
use crate::skips::{numeric_leaves, unknown_record_event, SkipLedger, UnknownRecord};
use crate::tool_action::{extract_local_tool_context, extract_tool_action, ToolActionInput};
use crate::tool_hash::{
    hash_args, json_bytes, mcp_server_name, normalize_tool_name, split_observed_tool_name,
    tool_identity,
};
use crate::types::{LocalToolContext, ParseResult, ParseStats, ParserContext, Sink, ToolCallDraft};
use crate::util::{slice_utf16, stated_duration_ms};

/// The elapsed time a rollout record states about itself, in milliseconds.
///
/// Codex writes its facts inside `payload` (the line itself is an envelope of
/// `timestamp` + `type`), so the probe reads the payload when there is one.
/// This is how `task_complete`'s `duration_ms` — the only number in the rollout
/// that says how long a turn actually took, and one no reader can reconstruct
/// from timestamps — reaches the wire without an arm existing per payload type.
fn record_duration_ms(obj: &Value) -> Option<u64> {
    stated_duration_ms(obj.get("payload").unwrap_or(obj))
}

/// `response_item` payload types this parser sees, understands, and deliberately
/// does not turn into events — each one duplicates an `event_msg` it already
/// reads. Listed rather than left to a catch-all so a payload type codex adds
/// tomorrow falls through as UNKNOWN instead of joining a silent decline.
const DECLINED_RESPONSE_ITEMS: &[&str] = &["message", "reasoning", "web_search_call"];

/// Both separators, on every platform. This parser runs on Windows, and a
/// rollout can be read on a machine other than the one that wrote it — so the
/// host's own separator is not the one to trust.
const SEPARATORS: [char; 2] = ['/', '\\'];

/// The files one `patch_apply_end` record names, made safe to leave the machine.
///
/// Codex keys `payload.changes` by ABSOLUTE path, and an absolute path is a home
/// directory: the username, and every directory above the checkout. Nothing
/// downstream floors this — `modelstat-redact` scrubs prose, and `files_touched`
/// is a `Vec<String>` no redactor walks — so the parser is the only place that
/// can make these safe. It is also the only place that knows the frame that
/// makes them meaningful: the session's own `cwd`.
///
///   * under `cwd` → the repo-relative path. That is the shape the rest of the
///     product already speaks — `git_files` emits it, and `components_from_slice`
///     (modelstat-pipeline) splits it on `/`.
///   * anywhere else (a dotfile, a second checkout), or no `cwd` known → the
///     final component alone. The directories above it are precisely the part
///     that leaks, and the file's own name is what "files touched" means.
///
/// Deduped, first-seen order kept — two files outside the repo can share a name.
/// No cap here: the wire's `FILES_TOUCHED_COUNT_MAX` is the one clamp.
fn safe_files_touched(paths: &[&str], cwd: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in paths {
        let path = raw.trim();
        let safe = repo_relative(path, cwd).unwrap_or_else(|| file_name(path));
        if safe.is_empty() || out.contains(&safe) {
            continue;
        }
        out.push(safe);
    }
    out
}

/// `path` stated relative to `root`, or `None` when it does not sit under it.
///
/// The root must end at a directory boundary, or `…/acme` would swallow
/// `…/acme-website/src/main.rs` and emit `ite/src/main.rs`. Every way this can
/// fail — a different drive, a symlinked or case-different root, no `cwd` at all
/// — degrades to the file's name alone: less context, never a leak.
fn repo_relative(path: &str, root: Option<&str>) -> Option<String> {
    let root = root?.trim_end_matches(SEPARATORS);
    if root.is_empty() {
        return None;
    }
    let rest = path.strip_prefix(root)?;
    if !rest.starts_with(SEPARATORS) {
        return None;
    }
    // One separator downstream, because a repo-relative path means the same
    // file whichever platform wrote it.
    Some(rest.trim_start_matches(SEPARATORS).replace('\\', "/"))
}

/// The final component of a path — the only part of it that is not a directory.
fn file_name(path: &str) -> String {
    path.rsplit(SEPARATORS).next().unwrap_or(path).to_string()
}

fn is_tool_call_payload(pt: &str) -> bool {
    matches!(
        pt,
        "function_call" | "local_shell_call" | "custom_tool_call" | "mcp_tool_call"
    )
}
fn is_tool_call_output_payload(pt: &str) -> bool {
    matches!(
        pt,
        "function_call_output"
            | "local_shell_call_output"
            | "custom_tool_call_output"
            | "mcp_tool_call_output"
    )
}
fn is_shell_tool_name(name: &str) -> bool {
    matches!(
        name,
        "shell" | "local_shell_call" | "exec_command" | "run_terminal_cmd"
    )
}

/// SPEC 0005 capture: one message's VERBATIM text + its length in chars.
/// Redaction is the only transformation — nothing is stripped, elided, or cut
/// short (the wire clamp is an extreme malicious-size guard, applied later).
/// Empty in → `(None, None)`, so a text-less line states no text rather than
/// an empty one.
fn message_text(raw: &str) -> (Option<String>, Option<u64>) {
    let text = raw.trim();
    if text.is_empty() {
        return (None, None);
    }
    let chars = text.chars().count() as u64;
    let cleaned = redact(text, None).text;
    if cleaned.is_empty() {
        (None, None)
    } else {
        (Some(cleaned), Some(chars))
    }
}

/// Drain the buffered `agent_message` prose for one round trip. Codex can
/// stream more than one message before a token count lands; they are joined as
/// written, in order.
fn take_message_text(buf: &mut Vec<String>) -> (Option<String>, Option<u64>) {
    if buf.is_empty() {
        return (None, None);
    }
    let joined = std::mem::take(buf).join("\n\n");
    message_text(&joined)
}

/// What one `token_count` line's `payload` says about token usage.
#[derive(Debug, PartialEq, Eq)]
enum CodexUsage {
    /// `info` absent or null. Legitimate: codex also emits `token_count`
    /// carrying only `rate_limits`. There is no usage to record, so the caller
    /// emits NO event (a zero-token assistant turn is a phantom turn).
    None,
    /// All four counters read cleanly.
    Mapped(TokenUsage),
    /// `info` is present but the usage object or one of its four counters is
    /// missing or non-numeric. `TokenUsageInfo::last_token_usage` is not
    /// optional upstream, so this can only mean the format moved under us.
    ///
    /// The event is still emitted — with NO `tokens` and with whatever numbers
    /// `info` did state carried verbatim in `tokens_unmapped`. Two earlier
    /// designs were worse in opposite directions: defaulting the missing counter
    /// to 0 was silent and permanently wrong (every codex event landed at zero
    /// tokens), and failing the parse was loud but unrecoverable — the scan
    /// never advanced the file's cursor, so the file re-parsed and re-failed
    /// every cycle forever while everything before the bad line re-shipped with
    /// it. Reporting "usage unknown, here are the numbers I found" is the only
    /// one of the three that is both honest and terminating.
    Drift(BTreeMap<String, u64>),
}

/// The token counters for ONE api call, read strictly from
/// `payload.info.last_token_usage`.
///
/// Codex nests token accounting two levels down — upstream is
/// `TokenCountEvent { info: Option<TokenUsageInfo>, rate_limits }` over
/// `TokenUsageInfo { total_token_usage, last_token_usage, model_context_window }`.
/// We read `last_token_usage` (the delta for THIS call) and NOT
/// `total_token_usage` (a running session total): every `token_count` line
/// becomes its own event and readers SUM them, so summing a cumulative counter
/// would grow quadratically with turn count.
fn codex_last_token_usage(p: &Value) -> CodexUsage {
    let info = match p.get("info") {
        None | Some(Value::Null) => return CodexUsage::None,
        Some(v) => v,
    };
    let Some(last) = info.get("last_token_usage") else {
        return CodexUsage::Drift(numeric_leaves(info));
    };
    let field = |name: &str| last.get(name).and_then(Value::as_u64);
    let (Some(input_tokens), Some(cached), Some(output_tokens), Some(reasoning)) = (
        field("input_tokens"),
        field("cached_input_tokens"),
        field("output_tokens"),
        field("reasoning_output_tokens"),
    ) else {
        return CodexUsage::Drift(numeric_leaves(info));
    };
    CodexUsage::Mapped(TokenUsage {
        // Codex counts cached input INSIDE `input_tokens` and reasoning INSIDE
        // `output_tokens` (upstream's `non_cached_input()` subtracts the former).
        // Our buckets are DISJOINT, so split them out rather than double-bill.
        input: input_tokens.saturating_sub(cached),
        output: output_tokens.saturating_sub(reasoning),
        // Codex discards cache-write counts before they reach the rollout JSONL
        // (openai/codex#32479), so 0 is the true value here, not a default.
        cache_creation: 0,
        cache_read: cached,
        reasoning,
    })
}

/// `payload.info.total_token_usage` — the conversation's running total at this
/// turn, in upstream's own declaration order.
///
/// This is NOT accounting input; the tokens an event carries still come from
/// `last_token_usage`. It is the turn's IDENTITY. Codex gives a rollout line no
/// uuid, and a fork file replays its ancestor's history with the timestamps
/// rewritten to the fork moment, so a copied turn shares nothing positional or
/// temporal with its original. The cumulative counter it does share, exactly —
/// see [`EventSource::CodexTurn`].
///
/// `None` when any counter is missing or non-numeric; the caller then falls back
/// to the positional key rather than minting a key from partial numbers.
fn codex_total_token_usage(p: &Value) -> Option<[u64; 4]> {
    let total = p.get("info")?.get("total_token_usage")?;
    let field = |name: &str| total.get(name).and_then(Value::as_u64);
    Some([
        field("input_tokens")?,
        field("cached_input_tokens")?,
        field("output_tokens")?,
        field("reasoning_output_tokens")?,
    ])
}

/// `rollout-<TS>-<UUID>.jsonl` → the session uuid.
pub fn derive_session_id_from_rollout_path(path: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"rollout-[0-9T-]+-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$").unwrap()
    });
    re.captures(path)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

struct Extracted {
    call_id: Option<String>,
    server: String,
    name: String,
    input: Value,
    failed: bool,
}

fn first_string(values: &[Option<&Value>]) -> Option<String> {
    for v in values {
        if let Some(Value::String(s)) = v {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}

/// Pin one tool-call payload to the wire shape. None when too malformed (no name).
fn extract_tool_call_payload(pt: &str, p: &Value) -> Option<Extracted> {
    let call_id = first_string(&[p.get("call_id"), p.get("id")]);
    let failed = p.get("status").and_then(Value::as_str) == Some("failed");

    if pt == "local_shell_call" {
        let action = match p.get("action") {
            Some(v) if v.is_object() => v.clone(),
            _ => Value::Null,
        };
        return Some(Extracted {
            call_id,
            server: "builtin".to_string(),
            name: "shell".to_string(),
            input: action,
            failed,
        });
    }

    let observed = first_string(&[p.get("name"), p.get("tool")])?;

    // custom_tool_call carries free-text `input` hashed verbatim; the rest carry
    // `arguments` as a JSON-encoded string (`arguments ?? input`).
    let mut input = if pt == "custom_tool_call" {
        p.get("input").cloned().unwrap_or(Value::Null)
    } else {
        match p.get("arguments") {
            Some(v) if !v.is_null() => v.clone(),
            _ => p.get("input").cloned().unwrap_or(Value::Null),
        }
    };
    if let Value::String(s) = &input {
        if s.trim().is_empty() {
            input = Value::Null;
        }
    }
    if pt != "custom_tool_call" {
        if let Value::String(s) = &input {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                input = parsed;
            }
        }
    }

    if is_shell_tool_name(&observed) {
        return Some(Extracted {
            call_id,
            server: "builtin".to_string(),
            name: "shell".to_string(),
            input,
            failed,
        });
    }

    if pt == "mcp_tool_call" {
        if let Some(server) = p.get("server").and_then(Value::as_str) {
            if !server.is_empty() {
                return Some(Extracted {
                    call_id,
                    server: mcp_server_name(server),
                    name: normalize_tool_name(&observed),
                    input,
                    failed,
                });
            }
        }
    }

    let (server, name) = split_observed_tool_name(&observed);
    Some(Extracted {
        call_id,
        server,
        name,
        input,
        failed,
    })
}

/// Best-effort error sniffing on an output payload's `output`/`result`.
fn output_indicates_error(p: &Value) -> bool {
    let out = p.get("output").or_else(|| p.get("result"));
    if let Some(Value::Object(o)) = out {
        if o.get("success") == Some(&Value::Bool(false))
            || o.get("is_error") == Some(&Value::Bool(true))
        {
            return true;
        }
    }
    false
}

pub fn parse_codex_rollout(ctx: &ParserContext) -> std::io::Result<ParseResult> {
    let mut sink = Sink::collect();
    let (tool_calls, script_contexts, stats, skips) = parse_inner(ctx, &mut sink)?;
    sink.flush();
    Ok(ParseResult {
        events: sink.take_collected(),
        tool_calls,
        script_contexts,
        stats,
        skipped_kinds: skips.into_counts(),
        source_file: ctx.source_file.clone(),
    })
}

pub fn parse_codex_rollout_streaming(
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

    let mut raw_lines: u64 = 0;
    let mut emitted: u64 = 0;
    let mut skipped: u64 = 0;
    let mut skips = SkipLedger::default();

    let file = File::open(&ctx.source_file)?;
    let mut lines = OffsetLines::new(BufReader::new(file), ctx.byte_offset_start);

    let mut session_id: Option<String> = derive_session_id_from_rollout_path(&ctx.source_file);
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    // Conversation turn ordinal (SPEC 0005) — the SAME quantity the other three
    // parsers emit: a turn starts at each typed prompt, and everything the agent
    // does in reply inherits that ordinal. It used to count usage-bearing
    // `token_count` lines instead, so one prompt's round trips walked the
    // ordinal upward (a fixture with a single prompt spanned turns 0, 1 and 2)
    // and the field meant something different for codex than for every other
    // agent — which a cross-agent reading of turn timing cannot survive.
    let mut turn_index: u64 = 0;
    let mut saw_user_prompt = false;
    let mut last_ts: Option<String> = None;
    // SPEC 0005: codex writes the assistant's prose on `event_msg`/
    // `agent_message` lines but its token counters on `event_msg`/`token_count`
    // lines, so the text is buffered here and attached to the next
    // usage-bearing assistant event — one event carrying BOTH, as the other
    // parsers produce. A round trip that only ran tools contributes nothing and
    // its event honestly carries no text. (Codex also repeats every message as
    // a `response_item`; capturing those too would double every message, so
    // the response_item path stays text-free.)
    let mut agent_text: Vec<String> = Vec::new();
    let mut open_calls: HashMap<String, usize> = HashMap::new();
    let mut pending_aggregate: BTreeMap<String, u64> = BTreeMap::new();
    // `total_token_usage` from the previous `token_count` line, to catch codex
    // restating one round trip's counters twice in a row.
    let mut prev_cumulative: Option<[u64; 4]> = None;

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

        if let Some(ts) = obj.get("timestamp").and_then(Value::as_str) {
            if !ts.is_empty() {
                last_ts = Some(ts.to_string());
            }
        }
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");

        if kind == "session_meta" {
            let id = obj.get("id").and_then(Value::as_str).or_else(|| {
                obj.get("payload")
                    .and_then(|p| p.get("id"))
                    .and_then(Value::as_str)
            });
            if let Some(id) = id {
                if Some(id) != session_id.as_deref() {
                    session_id = Some(id.to_string());
                    pending_aggregate.clear();
                    open_calls.clear();
                    // Each conversation runs its own cumulative counter, so the
                    // previous region's last value says nothing about this one.
                    prev_cumulative = None;
                }
            }
            continue;
        }
        if kind == "turn_context" {
            if let Some(c) = obj.get("cwd").and_then(Value::as_str).or_else(|| {
                obj.get("payload")
                    .and_then(|p| p.get("cwd"))
                    .and_then(Value::as_str)
            }) {
                cwd = Some(c.to_string());
            }
            if let Some(m) = obj.get("model").and_then(Value::as_str).or_else(|| {
                obj.get("payload")
                    .and_then(|p| p.get("model"))
                    .and_then(Value::as_str)
            }) {
                model = Some(m.to_string());
            }
            continue;
        }

        if kind == "response_item" {
            let payload = obj.get("payload");
            let pt = payload
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let line_ts = obj
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_string);

            if is_tool_call_payload(pt) && session_id.is_some() {
                let p = payload.unwrap();
                let extracted = match extract_tool_call_payload(pt, p) {
                    Some(e) => e,
                    None => {
                        skipped += 1;
                        continue;
                    }
                };
                // started_at must be deterministic: the line's ts, else the last
                // ts seen — and if neither exists, SKIP (mirrors claude-code).
                let ts = match line_ts.clone().or_else(|| last_ts.clone()) {
                    Some(t) => t,
                    None => {
                        skipped += 1;
                        continue;
                    }
                };
                let src_id = source_event_id(
                    &ctx.device_id,
                    &EventSource::File {
                        file: &ctx.source_file,
                        byte_offset: offset,
                    },
                );
                let hashes = hash_args(&extracted.input);
                let external_call_id = slice_utf16(
                    &extracted
                        .call_id
                        .clone()
                        .unwrap_or_else(|| tc_fallback_id(&src_id, 0)),
                    120,
                );
                if let Some((command, ctx_cwd)) = extract_local_tool_context(&ToolActionInput {
                    server: &extracted.server,
                    name: &extracted.name,
                    input: &extracted.input,
                    cwd: cwd.as_deref(),
                }) {
                    script_contexts.push(LocalToolContext {
                        external_call_id: external_call_id.clone(),
                        command,
                        cwd: ctx_cwd,
                    });
                }
                let action = extract_tool_action(&ToolActionInput {
                    server: &extracted.server,
                    name: &extracted.name,
                    input: &extracted.input,
                    cwd: cwd.as_deref(),
                });
                let draft = ToolCallDraft {
                    external_call_id,
                    session_id: session_id.clone().unwrap(),
                    source_event_id: src_id,
                    agent: "codex_cli".to_string(),
                    server: extracted.server.clone(),
                    name: extracted.name.clone(),
                    turn_index: Some(turn_index),
                    call_index: 0,
                    started_at: ts,
                    ended_at: None,
                    status: if extracted.failed { "error" } else { "unknown" }.to_string(),
                    args_hash: hashes.args_hash,
                    signature_hash: hashes.signature_hash,
                    args_bytes: hashes.args_bytes,
                    result_bytes: 0,
                    model: model.clone(),
                    action: Some(action),
                };
                tool_calls.push(draft);
                if let Some(cid) = &extracted.call_id {
                    open_calls.insert(cid.clone(), tool_calls.len() - 1);
                }
                let identity = tool_identity(&extracted.server, &extracted.name);
                *pending_aggregate.entry(identity).or_insert(0) += 1;
                continue;
            }

            if is_tool_call_output_payload(pt) {
                let p = payload.unwrap();
                let call_id = first_string(&[p.get("call_id"), p.get("id")]);
                let idx = call_id.as_ref().and_then(|c| open_calls.get(c).copied());
                match (call_id, idx) {
                    (Some(cid), Some(idx)) => {
                        open_calls.remove(&cid);
                        let ended = line_ts
                            .clone()
                            .or_else(|| last_ts.clone())
                            .unwrap_or_else(|| tool_calls[idx].started_at.clone());
                        let result_bytes = json_bytes(
                            p.get("output")
                                .or_else(|| p.get("result"))
                                .unwrap_or(&Value::Null),
                        );
                        let is_err = output_indicates_error(p);
                        let draft = &mut tool_calls[idx];
                        draft.ended_at = Some(ended);
                        draft.result_bytes = result_bytes;
                        if draft.status == "unknown" {
                            draft.status = if is_err { "error" } else { "success" }.to_string();
                        }
                    }
                    _ => {
                        skipped += 1;
                    }
                }
                continue;
            }

            // Modelled and declined: codex repeats every message and its
            // reasoning as a `response_item` alongside the `event_msg` this
            // parser reads, so taking these too would double each one. A
            // DECISION, not a failure — it stays out of the ledger.
            if DECLINED_RESPONSE_ITEMS.contains(&pt) {
                skipped += 1;
                continue;
            }
            // Anything else under `response_item` is a payload type nothing here
            // models.
            skips.drop_record(&ctx.source_file, &format!("response_item/{pt}"));
            match (
                session_id.clone(),
                line_ts.clone().or_else(|| last_ts.clone()),
            ) {
                (Some(sid), Some(ts)) => {
                    sink.push(unknown_record_event(UnknownRecord {
                        kind: pt,
                        source_event_id: source_event_id(
                            &ctx.device_id,
                            &EventSource::File {
                                file: &ctx.source_file,
                                byte_offset: offset,
                            },
                        ),
                        agent: "codex_cli",
                        provider: "openai",
                        session_id: sid,
                        ts,
                        turn_index: Some(turn_index),
                        duration_ms: record_duration_ms(&obj),
                        source_file: &ctx.source_file,
                        source_byte_offset: Some(offset),
                    }));
                    emitted += 1;
                }
                _ => skipped += 1,
            }
            continue;
        }

        if kind == "event_msg" {
            let payload = obj.get("payload");
            let ptype = payload.and_then(|p| p.get("type")).and_then(Value::as_str);
            let ptype = match ptype {
                Some(t) if !t.is_empty() => t,
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            let ts = match obj
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| last_ts.clone())
            {
                Some(t) => t,
                None => {
                    skipped += 1;
                    continue;
                }
            };

            if ptype == "token_count" {
                if session_id.is_none() {
                    skipped += 1;
                    continue;
                }
                let p = payload.unwrap();
                // No usage on this line (rate-limits-only `token_count`): emit
                // nothing. The pending tool-call aggregate stays pending and
                // rides the next assistant event, and `turn_index` does not move.
                let (tokens, tokens_unmapped) = match codex_last_token_usage(p) {
                    CodexUsage::None => {
                        skipped += 1;
                        continue;
                    }
                    CodexUsage::Mapped(t) => (Some(t), BTreeMap::new()),
                    CodexUsage::Drift(found) => {
                        // The round trip happened; only our reading of its
                        // counters failed. The event ships stating no usage
                        // (never a fabricated zero) and carrying the numbers the
                        // line did state, so the cost is recoverable later.
                        skips.drop_record(&ctx.source_file, "event_msg/token_count");
                        (None, found)
                    }
                };
                // The conversation's running total at this turn. It identifies
                // the turn across replays; it is never summed.
                let cumulative = codex_total_token_usage(p);
                // A counter that did not move means no api call happened between
                // the two lines, so the second states the SAME round trip a
                // second time (codex re-emits its final counters this way).
                // Summing both would bill that round trip twice.
                if cumulative.is_some() && cumulative == prev_cumulative {
                    skipped += 1;
                    continue;
                }
                prev_cumulative = cumulative;
                let slug = guess_repo_slug_from_path(cwd.as_deref());
                let git = path_guessed_git_context(slug.clone(), None);
                // The prose codex streamed for this round trip, verbatim.
                let (content_excerpt, content_bytes) = take_message_text(&mut agent_text);
                let sid = session_id.clone().unwrap();
                sink.push(RawEvent {
                    // Replay-stable when codex states the cumulative counter;
                    // positional only when it does not, which no observed line
                    // does — a fabricated key would be worse than a positional
                    // one, because it would collapse UNRELATED turns.
                    source_event_id: match cumulative {
                        Some(cumulative) => source_event_id(
                            &ctx.device_id,
                            &EventSource::CodexTurn {
                                session_id: &sid,
                                cumulative,
                            },
                        ),
                        None => source_event_id(
                            &ctx.device_id,
                            &EventSource::File {
                                file: &ctx.source_file,
                                byte_offset: offset,
                            },
                        ),
                    },
                    ts,
                    kind: "assistant_message".to_string(),
                    agent: "codex_cli".to_string(),
                    provider: "openai".to_string(),
                    model: model.clone(),
                    session_id: sid,
                    turn_index: Some(turn_index),
                    parent_event_id: None,
                    cwd: cwd.clone(),
                    git,
                    tokens,
                    tokens_unmapped,
                    duration_ms: None,
                    tool_calls: std::mem::take(&mut pending_aggregate),
                    files_touched: Vec::new(),
                    content_excerpt,
                    content_bytes,
                    references: None,
                    source_file: Some(ctx.source_file.clone()),
                    source_byte_offset: Some(offset),
                    redactions: Default::default(),
                });
                emitted += 1;
                continue;
            }
            if ptype == "agent_message" {
                // Buffered, not emitted: the assistant's event is the
                // usage-bearing `token_count` line above.
                if let Some(t) = payload
                    .and_then(|p| p.get("message"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    agent_text.push(t.to_string());
                }
                skipped += 1;
                continue;
            }
            if ptype == "user_message" {
                if session_id.is_none() {
                    skipped += 1;
                    continue;
                }
                // What the developer actually typed. `event_msg` carries the
                // typed prompt; the parallel `response_item` user rows also
                // include harness-injected content, so this is the honest one.
                let (content_excerpt, content_bytes) = message_text(
                    payload
                        .and_then(|p| p.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
                // A real (typed) prompt starts a new turn, exactly as in the
                // claude_code, pi and cursor parsers; anything before the first
                // one sits in turn 0.
                if content_excerpt.is_some() {
                    if saw_user_prompt {
                        turn_index += 1;
                    }
                    saw_user_prompt = true;
                }
                sink.push(RawEvent {
                    source_event_id: source_event_id(
                        &ctx.device_id,
                        &EventSource::File {
                            file: &ctx.source_file,
                            byte_offset: offset,
                        },
                    ),
                    ts,
                    kind: "user_message".to_string(),
                    agent: "codex_cli".to_string(),
                    provider: "openai".to_string(),
                    model: model.clone(),
                    session_id: session_id.clone().unwrap(),
                    turn_index: Some(turn_index),
                    parent_event_id: None,
                    cwd: cwd.clone(),
                    git: None,
                    tokens: None,
                    tokens_unmapped: BTreeMap::new(),
                    duration_ms: None,
                    tool_calls: BTreeMap::new(),
                    files_touched: Vec::new(),
                    content_excerpt,
                    content_bytes,
                    references: None,
                    source_file: Some(ctx.source_file.clone()),
                    source_byte_offset: Some(offset),
                    redactions: Default::default(),
                });
                emitted += 1;
                continue;
            }
            if ptype == "patch_apply_end" {
                // Codex STATES which files it changed: `payload.changes` is
                // keyed by path, one key per file. `files_touched` had no
                // producer in any parser until this arm, so the taxonomy
                // `components` dimension the server derives from it was computed
                // over an empty list on every session ever uploaded.
                //
                // The values are unified diffs — source code — and this arm
                // ships none of them: the record's own word plus the file names
                // is everything it says.
                let changed: Vec<&str> = payload
                    .and_then(|p| p.get("changes"))
                    .and_then(Value::as_object)
                    .map(|m| m.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                // A fork rollout replays these records too, so the same edit
                // must not land twice. `call_id` is codex's own globally unique
                // name for the call and survives the copy verbatim — the same
                // shape of identity Claude Code's line uuid provides.
                let call_id = payload
                    .and_then(|p| p.get("call_id"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                match session_id.clone() {
                    Some(sid) => {
                        sink.push(RawEvent {
                            source_event_id: source_event_id(
                                &ctx.device_id,
                                &match call_id {
                                    Some(line_uuid) => EventSource::LineUuid { line_uuid },
                                    None => EventSource::File {
                                        file: &ctx.source_file,
                                        byte_offset: offset,
                                    },
                                },
                            ),
                            ts,
                            kind: "patch_apply_end".to_string(),
                            agent: "codex_cli".to_string(),
                            provider: "openai".to_string(),
                            model: model.clone(),
                            session_id: sid,
                            turn_index: Some(turn_index),
                            parent_event_id: None,
                            cwd: cwd.clone(),
                            git: None,
                            tokens: None,
                            tokens_unmapped: BTreeMap::new(),
                            duration_ms: record_duration_ms(&obj),
                            tool_calls: BTreeMap::new(),
                            files_touched: safe_files_touched(&changed, cwd.as_deref()),
                            content_excerpt: None,
                            content_bytes: None,
                            references: None,
                            source_file: Some(ctx.source_file.clone()),
                            source_byte_offset: Some(offset),
                            redactions: Default::default(),
                        });
                        emitted += 1;
                    }
                    None => skipped += 1,
                }
                continue;
            }
            // An `event_msg` payload type nothing here models. `ts` is already
            // resolved above, so the only question left is the session.
            skips.drop_record(&ctx.source_file, &format!("event_msg/{ptype}"));
            match session_id.clone() {
                Some(sid) => {
                    sink.push(unknown_record_event(UnknownRecord {
                        kind: ptype,
                        source_event_id: source_event_id(
                            &ctx.device_id,
                            &EventSource::File {
                                file: &ctx.source_file,
                                byte_offset: offset,
                            },
                        ),
                        agent: "codex_cli",
                        provider: "openai",
                        session_id: sid,
                        ts,
                        turn_index: Some(turn_index),
                        duration_ms: record_duration_ms(&obj),
                        source_file: &ctx.source_file,
                        source_byte_offset: Some(offset),
                    }));
                    emitted += 1;
                }
                None => skipped += 1,
            }
            continue;
        }

        // A top-level `type` nothing here models — the envelope itself is new.
        skips.drop_record(&ctx.source_file, kind);
        match (session_id.clone(), last_ts.clone()) {
            (Some(sid), Some(ts)) => {
                sink.push(unknown_record_event(UnknownRecord {
                    kind,
                    source_event_id: source_event_id(
                        &ctx.device_id,
                        &EventSource::File {
                            file: &ctx.source_file,
                            byte_offset: offset,
                        },
                    ),
                    agent: "codex_cli",
                    provider: "openai",
                    session_id: sid,
                    ts,
                    turn_index: Some(turn_index),
                    duration_ms: record_duration_ms(&obj),
                    source_file: &ctx.source_file,
                    source_byte_offset: Some(offset),
                }));
                emitted += 1;
            }
            _ => skipped += 1,
        }
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
    use serde_json::json;

    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A rollout of `lines` at a path codex's own naming rule matches. Shapes
    /// only — every value is fabricated to match the FORM codex writes.
    fn rollout(lines: &[Value]) -> String {
        let dir = std::env::temp_dir().join(format!(
            "modelstat-codex-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path =
            dir.join("rollout-2026-08-05T13-58-57-019fd1ca-816d-7af2-9332-a6db0bfc4d25.jsonl");
        let text: String = lines
            .iter()
            .map(|l| format!("{}\n", serde_json::to_string(l).unwrap()))
            .collect();
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn line(ts: &str, ptype: &str, extra: Value) -> Value {
        let mut payload = json!({ "type": ptype });
        if let (Some(p), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                p.insert(k.clone(), v.clone());
            }
        }
        json!({ "timestamp": ts, "type": "event_msg", "payload": payload })
    }

    fn usage(input: u64) -> Value {
        json!({ "info": { "last_token_usage": {
            "input_tokens": input, "cached_input_tokens": 0,
            "output_tokens": 10, "reasoning_output_tokens": 0
        }}})
    }

    /// The ordinal counts TYPED PROMPTS, the same quantity claude_code, pi and
    /// cursor emit. It used to count usage-bearing `token_count` lines, so one
    /// prompt whose reply took three round trips reported three different turns
    /// — and `turn_index` meant one thing for codex and another for everyone
    /// else, which no cross-agent reading of turn timing survives.
    #[test]
    fn the_turn_ordinal_advances_at_a_typed_prompt_not_at_an_api_round_trip() {
        let path = rollout(&[
            json!({ "timestamp": "2026-08-05T11:58:57.508Z", "type": "session_meta",
                    "payload": { "id": "019fd1ca-816d-7af2-9332-a6db0bfc4d25" } }),
            line(
                "2026-08-05T11:58:59.076Z",
                "user_message",
                json!({ "message": "first ask" }),
            ),
            line("2026-08-05T11:59:02.811Z", "token_count", usage(100)),
            line("2026-08-05T11:59:03.811Z", "token_count", usage(200)),
            line("2026-08-05T11:59:04.046Z", "token_count", usage(300)),
            line(
                "2026-08-05T12:00:00.000Z",
                "user_message",
                json!({ "message": "second ask" }),
            ),
            line("2026-08-05T12:00:04.000Z", "token_count", usage(400)),
        ]);
        let res = parse_codex_rollout(&ParserContext::new("dev_1", &path)).unwrap();
        let seen: Vec<(&str, Option<u64>)> = res
            .events
            .iter()
            .map(|e| (e.kind.as_str(), e.turn_index))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("user_message", Some(0)),
                ("assistant_message", Some(0)),
                ("assistant_message", Some(0)),
                ("assistant_message", Some(0)),
                ("user_message", Some(1)),
                ("assistant_message", Some(1)),
            ],
            "three round trips answering one prompt are all that prompt's turn"
        );
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    /// `task_complete` is the only record in a rollout that says how long a turn
    /// took, and it is codex's OWN measurement — nothing downstream can derive
    /// it. The parser models no arm for the record and still carries the number:
    /// a stated duration is a structural field, like the instant and the ids.
    #[test]
    fn codex_s_own_stated_turn_duration_survives_the_record_being_unmodelled() {
        let path = rollout(&[
            json!({ "timestamp": "2026-08-05T11:58:57.508Z", "type": "session_meta",
                    "payload": { "id": "019fd1ca-816d-7af2-9332-a6db0bfc4d25" } }),
            line(
                "2026-08-05T11:58:59.076Z",
                "user_message",
                json!({ "message": "ask" }),
            ),
            line(
                "2026-08-05T11:59:04.063Z",
                "task_complete",
                json!({ "duration_ms": 6556, "time_to_first_token_ms": 3076,
                        "started_at": 1_785_931_137u64, "completed_at": 1_785_931_144u64 }),
            ),
        ]);
        let res = parse_codex_rollout(&ParserContext::new("dev_1", &path)).unwrap();
        let done = res
            .events
            .iter()
            .find(|e| e.kind == "task_complete")
            .expect("the record ships as an event");
        assert_eq!(
            done.duration_ms,
            Some(6556),
            "codex's own number, as stated"
        );
        assert_eq!(done.ts, "2026-08-05T11:59:04.063Z");
        assert_eq!(done.turn_index, Some(0), "it closes the prompt's own turn");
        assert!(
            done.content_excerpt.is_none(),
            "an unmodelled record still ships none of what it said"
        );
        assert_eq!(
            res.skipped_kinds.get("event_msg/task_complete"),
            Some(&1),
            "reading a structural field is not the same as modelling the record"
        );
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    /// The invariant, stated as a test: whatever comes out of the path rule, a
    /// home directory does not. Codex hands us absolute paths and nothing
    /// downstream floors `files_touched`, so "no `/Users/`, never rooted" has to
    /// hold HERE or it holds nowhere.
    #[test]
    fn a_changed_file_never_carries_the_directories_above_it() {
        let cases: &[(&str, Option<&str>, &str, &str)] = &[
            (
                "/Users/dev/Projects/acme/src/lib.rs",
                Some("/Users/dev/Projects/acme"),
                "src/lib.rs",
                "under cwd → repo-relative",
            ),
            (
                "/Users/dev/Projects/acme/src/lib.rs",
                Some("/Users/dev/Projects/acme/"),
                "src/lib.rs",
                "a trailing separator on cwd changes nothing",
            ),
            (
                "/Users/dev/.zshrc",
                Some("/Users/dev/Projects/acme"),
                ".zshrc",
                "outside cwd → the file's own name, nothing above it",
            ),
            (
                "/Users/dev/Projects/globex/README.md",
                None,
                "README.md",
                "no cwd known → the same, the name alone",
            ),
            (
                "/Users/dev/Projects/acme-website/src/main.rs",
                Some("/Users/dev/Projects/acme"),
                "main.rs",
                "a sibling whose name STARTS with cwd is not under it",
            ),
            (
                "C:\\Users\\dev\\Projects\\acme\\src\\main.rs",
                Some("C:\\Users\\dev\\Projects\\acme"),
                "src/main.rs",
                "windows behaves identically, and states one separator",
            ),
            (
                "D:\\scratch\\notes.md",
                Some("C:\\Users\\dev\\Projects\\acme"),
                "notes.md",
                "another drive is outside cwd",
            ),
        ];
        for (path, cwd, want, why) in cases {
            let got = safe_files_touched(&[path], *cwd);
            assert_eq!(got, vec![want.to_string()], "{why}");
            let only = &got[0];
            assert!(
                !only.contains("/Users/") && !only.contains("\\Users\\"),
                "a home directory reached the wire via {path:?}: {only:?}"
            );
            assert!(
                !only.starts_with('/') && !only.starts_with('\\'),
                "an absolute path reached the wire via {path:?}: {only:?}"
            );
        }
    }

    /// Codex re-applies patches to the same file many times in a turn, and two
    /// files outside the repo can share a name. First-seen order is the record's
    /// own order, so the list reads as the sequence of edits it was.
    #[test]
    fn repeated_files_collapse_and_the_first_sighting_sets_the_order() {
        let cwd = Some("/Users/dev/Projects/acme");
        let got = safe_files_touched(
            &[
                "/Users/dev/Projects/acme/src/main.rs",
                "/Users/dev/Projects/acme/Cargo.toml",
                "/Users/dev/Projects/acme/src/main.rs",
                "/Users/dev/Projects/globex/notes.md",
                "/Users/dev/Projects/acme-website/notes.md",
                "",
            ],
            cwd,
        );
        assert_eq!(
            got,
            vec![
                "src/main.rs".to_string(),
                "Cargo.toml".to_string(),
                "notes.md".to_string(),
            ],
            "two out-of-repo files named notes.md are one name once, and an \
             empty key states nothing"
        );
    }

    /// End to end over the record's real shape (fabricated values). The kind
    /// codex writes is what ships, the paths are repo-relative, the unified
    /// diffs stay on the machine — and because the arm MODELS the record, the
    /// kind leaves the skip ledger.
    #[test]
    fn codex_s_own_account_of_which_files_it_changed_reaches_the_wire() {
        let path = rollout(&[
            json!({ "timestamp": "2026-08-05T11:58:57.508Z", "type": "session_meta",
                    "payload": { "id": "019fd1ca-816d-7af2-9332-a6db0bfc4d25" } }),
            json!({ "timestamp": "2026-08-05T11:58:58.000Z", "type": "turn_context",
                    "payload": { "cwd": "/Users/dev/Projects/acme", "model": "gpt-5-codex" } }),
            line(
                "2026-08-05T11:58:59.076Z",
                "user_message",
                json!({ "message": "rename the helper" }),
            ),
            line(
                "2026-08-05T11:59:01.000Z",
                "patch_apply_end",
                json!({
                    "call_id": "call_0123", "turn_id": "turn_0123",
                    "stdout": "Success. Updated the following files:\nM src/lib.rs\n",
                    "stderr": "", "success": true, "status": "completed",
                    "changes": {
                        "/Users/dev/Projects/acme/src/lib.rs": {
                            "type": "update",
                            "unified_diff": "@@ -1,1 +1,1 @@\n-fn helper() {}\n+fn assist() {}\n",
                            "move_path": null
                        },
                        "/Users/dev/.config/acme/config.toml": {
                            "type": "add",
                            "unified_diff": "@@ -0,0 +1 @@\n+renamed = true\n",
                            "move_path": null
                        }
                    }
                }),
            ),
        ]);
        let res = parse_codex_rollout(&ParserContext::new("dev_1", &path)).unwrap();
        let patch = res
            .events
            .iter()
            .find(|e| e.kind == "patch_apply_end")
            .expect("the record codex writes ships under its own word");
        assert_eq!(
            patch.files_touched,
            vec!["src/lib.rs".to_string(), "config.toml".to_string()],
            "the repo file keeps its path; the dotfile keeps only its name"
        );
        assert!(
            patch.content_excerpt.is_none() && patch.content_bytes.is_none(),
            "a unified diff is source code and this arm ships none of it"
        );
        assert_eq!(patch.turn_index, Some(0), "it is the typed prompt's turn");
        assert_eq!(patch.agent, "codex_cli");
        assert_eq!(patch.provider, "openai");
        assert_eq!(patch.cwd.as_deref(), Some("/Users/dev/Projects/acme"));
        assert_eq!(
            res.skipped_kinds.get("event_msg/patch_apply_end"),
            None,
            "an arm that models the record must leave the skip ledger"
        );
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    /// The same edit, replayed into a fork's rollout, is ONE edit. `call_id` is
    /// what says so — keyed by byte offset, the fork would report the file as
    /// changed a second time and the components dimension would count it twice.
    #[test]
    fn a_replayed_patch_record_keeps_the_original_s_event_id() {
        let patch = |ts: &str| {
            line(
                ts,
                "patch_apply_end",
                json!({
                    "call_id": "exec-e1472a0d", "turn_id": "turn_1",
                    "stdout": "Success. Updated the following files:\nM src/lib.rs\n",
                    "stderr": "", "success": true, "status": "completed",
                    "changes": { "/Users/dev/Projects/acme/src/lib.rs": {
                        "type": "update", "unified_diff": "@@\n", "move_path": null } }
                }),
            )
        };
        let meta = json!({ "timestamp": "2026-08-05T11:58:57.508Z", "type": "session_meta",
                "payload": { "id": "019fd1ca-816d-7af2-9332-a6db0bfc4d25" } });
        let ctx_line = json!({ "timestamp": "2026-08-05T11:58:58.000Z", "type": "turn_context",
                "payload": { "cwd": "/Users/dev/Projects/acme", "model": "gpt-5-codex" } });
        // The session's own rollout.
        let own = rollout(&[
            meta.clone(),
            ctx_line.clone(),
            patch("2026-08-05T11:59:01.000Z"),
        ]);
        // A fork replaying it: extra lines ahead of the record move its byte
        // offset, and codex stamps the copy at the fork moment.
        let fork = rollout(&[
            meta.clone(),
            ctx_line.clone(),
            line(
                "2026-08-05T12:41:18.000Z",
                "user_message",
                json!({ "message": "carry on" }),
            ),
            patch("2026-08-05T12:41:18.204Z"),
        ]);
        let id_of = |p: &str| {
            parse_codex_rollout(&ParserContext::new("dev_1", p))
                .unwrap()
                .events
                .iter()
                .find(|e| e.kind == "patch_apply_end")
                .expect("the patch record ships")
                .source_event_id
                .clone()
        };
        assert_eq!(
            id_of(&own),
            id_of(&fork),
            "one edit, one event — whichever rollout it was read from"
        );
        for p in [&own, &fork] {
            let _ = std::fs::remove_dir_all(std::path::Path::new(p).parent().unwrap());
        }
    }

    #[test]
    fn reads_the_per_call_delta_not_the_cumulative_total() {
        // The regression that made EVERY codex event land with 0 tokens: counters
        // were read from the payload root, but codex nests them under
        // `info.last_token_usage`. `total_token_usage` is deliberately larger here,
        // so reading the wrong one is visible instead of merely plausible.
        let p = json!({
            "type": "token_count",
            "info": {
                "total_token_usage": {
                    "input_tokens": 9000, "cached_input_tokens": 0,
                    "cache_write_input_tokens": 0, "output_tokens": 4000,
                    "reasoning_output_tokens": 0, "total_tokens": 13000
                },
                "last_token_usage": {
                    "input_tokens": 100, "cached_input_tokens": 40,
                    "cache_write_input_tokens": 0, "output_tokens": 1000,
                    "reasoning_output_tokens": 600, "total_tokens": 1100
                },
                "model_context_window": 272_000
            },
            "rate_limits": null
        });
        let CodexUsage::Mapped(t) = codex_last_token_usage(&p) else {
            panic!("usage present");
        };
        // Disjoint buckets: cached is carved out of input, reasoning out of output.
        assert_eq!(t.input, 60, "input excludes the 40 cached");
        assert_eq!(t.output, 400, "output excludes the 600 reasoning");
        assert_eq!(t.cache_read, 40);
        assert_eq!(t.reasoning, 600);
        assert_eq!(t.cache_creation, 0);
        // Totals are preserved against codex's inclusive counters.
        assert_eq!(t.input + t.cache_read, 100);
        assert_eq!(t.output + t.reasoning, 1000);
    }

    #[test]
    fn rate_limits_only_token_count_reports_no_usage() {
        // Codex emits `token_count` carrying only `rate_limits`. That is not a
        // zero-token turn — it is no turn at all, so the caller emits no event.
        let p = json!({ "type": "token_count", "rate_limits": { "primary_used_percent": 12.5 } });
        assert_eq!(codex_last_token_usage(&p), CodexUsage::None);
        let explicit_null = json!({ "type": "token_count", "info": null });
        assert_eq!(codex_last_token_usage(&explicit_null), CodexUsage::None);
    }

    /// Moved counters are REPORTED, never zeroed and never thrown.
    ///
    /// Zeroing is silent and permanently wrong. Throwing was loud but did not
    /// terminate: the scan skipped the cursor push for a file whose parse
    /// errored, so the file re-parsed, re-failed, and re-shipped its whole
    /// readable prefix every cycle, forever. What survives is the numbers.
    #[test]
    fn moved_counters_are_reported_not_zeroed_and_not_thrown() {
        let renamed = json!({
            "type": "token_count",
            "info": { "last_token_usage": { "prompt_tokens": 100, "completion_tokens": 50 } }
        });
        let CodexUsage::Drift(found) = codex_last_token_usage(&renamed) else {
            panic!("a renamed counter is drift");
        };
        // The upstream's OWN names, at their own paths — nothing normalised, so
        // a reader can grep codex's source for the string we reported.
        assert_eq!(found["last_token_usage.prompt_tokens"], 100);
        assert_eq!(found["last_token_usage.completion_tokens"], 50);

        let usage_gone = json!({ "type": "token_count", "info": { "model_context_window": 1 } });
        let CodexUsage::Drift(found) = codex_last_token_usage(&usage_gone) else {
            panic!("a vanished usage object is drift");
        };
        assert_eq!(found["model_context_window"], 1);

        // A counter that stopped being a number takes the same path — and the
        // string it became is NOT carried: only numeric leaves ride the wire.
        let not_a_number = json!({
            "type": "token_count",
            "info": { "last_token_usage": {
                "input_tokens": "100", "cached_input_tokens": 0,
                "output_tokens": 50, "reasoning_output_tokens": 0
            }}
        });
        let CodexUsage::Drift(found) = codex_last_token_usage(&not_a_number) else {
            panic!("a non-numeric counter is drift");
        };
        assert_eq!(found["last_token_usage.output_tokens"], 50);
        assert!(
            !found.contains_key("last_token_usage.input_tokens"),
            "a string leaf must never ride an unvalidated shape onto the wire"
        );
    }
}
