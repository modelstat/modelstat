//! Codex CLI rollout parser — a byte-for-byte port of
//! `packages/parsers/src/codex/index.ts`.
//!
//! Tool calls come from `response_item` payloads and become drafts (never
//! events); the aggregate identity→count map attaches to the next emitted
//! assistant event. Token accounting stores DISJOINT buckets (input excl. cache,
//! output excl. reasoning) — the double-billing fix (feature §7.1).
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
                let input_tokens = p.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                let cached = p
                    .get("cached_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let output_tokens = p.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                let reasoning = p
                    .get("reasoning_output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
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
                    tokens: Some(TokenUsage {
                        input: input_tokens.saturating_sub(cached),
                        output: output_tokens.saturating_sub(reasoning),
                        cache_creation: 0,
                        cache_read: cached,
                        reasoning,
                    }),
                    duration_ms: None,
                    tool_calls: std::mem::take(&mut pending_aggregate),
                    files_touched: Vec::new(),
                    content_excerpt: None,
                    references: None,
                    source_file: Some(ctx.source_file.clone()),
                    source_byte_offset: Some(offset),
                    pricing_mode: None,
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
                    references: None,
                    source_file: Some(ctx.source_file.clone()),
                    source_byte_offset: Some(offset),
                    pricing_mode: None,
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
