//! Codex CLI rollout parser — a byte-for-byte port of
//! `packages/parsers/src/codex/index.ts`.
//!
//! Tool calls come from `response_item` payloads and become drafts (never
//! events); the aggregate identity→count map attaches to the next emitted
//! assistant event. Token accounting stores DISJOINT buckets (input excl. cache,
//! output excl. reasoning) — the double-billing fix (feature §7.1).
//!
//! Token counters live at `payload.info.last_token_usage` — see
//! [`codex_last_token_usage`] for why that exact path, and why a missing counter
//! is a hard error rather than a zero.
//!
//! PARITY: the TS event_msg path falls back to `new Date().toISOString()` when a
//! line has no timestamp. That is non-deterministic and non-replayable, so the
//! Rust port falls back to the last-seen line timestamp instead (deterministic);
//! codex always writes timestamps, so this never differs in practice.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufReader;
use std::sync::OnceLock;

use modelstat_wire::{
    source_event_id, tc_fallback_id, EventSource, GitContext, RawEvent, TokenUsage,
};
use regex::Regex;
use serde_json::Value;

use crate::git::guess_repo_slug_from_path;
use crate::line_reader::OffsetLines;
use crate::tool_action::{extract_local_tool_context, extract_tool_action, ToolActionInput};
use crate::tool_hash::{
    hash_args, json_bytes, mcp_server_name, normalize_tool_name, split_observed_tool_name,
    tool_identity,
};
use crate::types::{LocalToolContext, ParseResult, ParseStats, ParserContext, Sink, ToolCallDraft};
use crate::util::slice_utf16;

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

fn schema_drift(detail: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "codex token_count schema drift: {detail}. Refusing to record zero tokens — \
             a silently-zeroed count under-reports real spend. Update the parser."
        ),
    )
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
///
/// `Ok(None)` — `info` absent or null. Legitimate: codex also emits
/// `token_count` carrying only `rate_limits`. There is no usage to record, so
/// the caller emits NO event (a zero-token assistant turn is a phantom turn).
///
/// `Err` — `info` is present but the usage object or any of its four counters is
/// missing or non-numeric. `TokenUsageInfo::last_token_usage` is not optional
/// upstream, so this can only mean the format changed under us. That is a HARD
/// failure by design: the daemon holds its cursor and retries, which is loud and
/// recoverable, whereas defaulting to 0 is silent and permanently wrong. This is
/// exactly the bug that made every codex event land with 0 tokens.
fn codex_last_token_usage(p: &Value) -> std::io::Result<Option<TokenUsage>> {
    let info = match p.get("info") {
        None | Some(Value::Null) => return Ok(None),
        Some(v) => v,
    };
    let last = info
        .get("last_token_usage")
        .ok_or_else(|| schema_drift("payload.info.last_token_usage is missing"))?;
    let field = |name: &str| -> std::io::Result<u64> {
        last.get(name).and_then(Value::as_u64).ok_or_else(|| {
            schema_drift(&format!(
                "payload.info.last_token_usage.{name} is missing or not a number"
            ))
        })
    };
    let input_tokens = field("input_tokens")?;
    let cached = field("cached_input_tokens")?;
    let output_tokens = field("output_tokens")?;
    let reasoning = field("reasoning_output_tokens")?;
    Ok(Some(TokenUsage {
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
    }))
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
    let (tool_calls, script_contexts, stats) = parse_inner(ctx, &mut sink)?;
    sink.flush();
    Ok(ParseResult {
        events: sink.take_collected(),
        tool_calls,
        script_contexts,
        stats,
        source_file: ctx.source_file.clone(),
    })
}

pub fn parse_codex_rollout_streaming(
    ctx: &ParserContext,
    emit: &mut dyn FnMut(Vec<RawEvent>),
) -> std::io::Result<ParseResult> {
    let mut sink = Sink::stream(emit);
    let (tool_calls, script_contexts, stats) = parse_inner(ctx, &mut sink)?;
    sink.flush();
    Ok(ParseResult {
        events: Vec::new(),
        tool_calls,
        script_contexts,
        stats,
        source_file: ctx.source_file.clone(),
    })
}

fn parse_inner(
    ctx: &ParserContext,
    sink: &mut Sink,
) -> std::io::Result<(Vec<ToolCallDraft>, Vec<LocalToolContext>, ParseStats)> {
    let mut tool_calls: Vec<ToolCallDraft> = Vec::new();
    let mut script_contexts: Vec<LocalToolContext> = Vec::new();

    let mut raw_lines: u64 = 0;
    let mut emitted: u64 = 0;
    let mut skipped: u64 = 0;

    let file = File::open(&ctx.source_file)?;
    let mut lines = OffsetLines::new(BufReader::new(file), ctx.byte_offset_start);

    let mut session_id: Option<String> = derive_session_id_from_rollout_path(&ctx.source_file);
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    let mut turn_index: u64 = 0;
    let mut last_ts: Option<String> = None;
    let mut open_calls: HashMap<String, usize> = HashMap::new();
    let mut pending_aggregate: BTreeMap<String, u64> = BTreeMap::new();

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

            // message / reasoning / web_search_call — not tool data.
            skipped += 1;
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
                let Some(tokens) = codex_last_token_usage(p)? else {
                    skipped += 1;
                    continue;
                };
                let slug = guess_repo_slug_from_path(cwd.as_deref());
                let git = slug.as_ref().map(|s| GitContext {
                    remote_url: None,
                    remote_host: if s.contains('/') {
                        Some("github.com".to_string())
                    } else {
                        None
                    },
                    remote_slug: Some(s.clone()),
                    branch: None,
                });
                sink.push(RawEvent {
                    source_event_id: source_event_id(
                        &ctx.device_id,
                        &EventSource::File {
                            file: &ctx.source_file,
                            byte_offset: offset,
                        },
                    ),
                    ts,
                    kind: "assistant_message".to_string(),
                    agent: "codex_cli".to_string(),
                    provider: "openai".to_string(),
                    model: model.clone(),
                    session_id: session_id.clone().unwrap(),
                    turn_index: Some(turn_index),
                    parent_event_id: None,
                    cwd: cwd.clone(),
                    git,
                    tokens: Some(tokens),
                    duration_ms: None,
                    tool_calls: std::mem::take(&mut pending_aggregate),
                    files_touched: Vec::new(),
                    content_excerpt: None,
                    content_bytes: None,
                    references: None,
                    source_file: Some(ctx.source_file.clone()),
                    source_byte_offset: Some(offset),
                    pricing_mode: ctx.pricing_mode.clone(),
                });
                emitted += 1;
                turn_index += 1;
                continue;
            }
            if ptype == "user_message" {
                if session_id.is_none() {
                    skipped += 1;
                    continue;
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
                    turn_index: None,
                    parent_event_id: None,
                    cwd: cwd.clone(),
                    git: None,
                    tokens: None,
                    duration_ms: None,
                    tool_calls: BTreeMap::new(),
                    files_touched: Vec::new(),
                    content_excerpt: None,
                    content_bytes: None,
                    references: None,
                    source_file: Some(ctx.source_file.clone()),
                    source_byte_offset: Some(offset),
                    pricing_mode: ctx.pricing_mode.clone(),
                });
                emitted += 1;
                continue;
            }
            skipped += 1;
            continue;
        }

        skipped += 1;
    }

    Ok((
        tool_calls,
        script_contexts,
        ParseStats {
            raw_lines,
            emitted_events: emitted,
            skipped,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let t = codex_last_token_usage(&p).unwrap().expect("usage present");
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
        assert!(codex_last_token_usage(&p).unwrap().is_none());
        let explicit_null = json!({ "type": "token_count", "info": null });
        assert!(codex_last_token_usage(&explicit_null).unwrap().is_none());
    }

    #[test]
    fn moved_counters_error_instead_of_recording_zeros() {
        // Fail loud on upstream schema drift: silently zeroing under-reports spend.
        let renamed = json!({
            "type": "token_count",
            "info": { "last_token_usage": { "prompt_tokens": 100, "completion_tokens": 50 } }
        });
        let err = codex_last_token_usage(&renamed).expect_err("must not default to 0");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("input_tokens"), "{err}");

        let usage_gone = json!({ "type": "token_count", "info": { "model_context_window": 1 } });
        let err = codex_last_token_usage(&usage_gone).expect_err("must not default to 0");
        assert!(err.to_string().contains("last_token_usage"), "{err}");

        let not_a_number = json!({
            "type": "token_count",
            "info": { "last_token_usage": {
                "input_tokens": "100", "cached_input_tokens": 0,
                "output_tokens": 50, "reasoning_output_tokens": 0
            }}
        });
        assert!(codex_last_token_usage(&not_a_number).is_err());
    }
}
