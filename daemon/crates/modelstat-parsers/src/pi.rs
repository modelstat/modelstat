//! pi harness JSONL parser — a byte-for-byte port of
//! `packages/parsers/src/pi/index.ts`.
//!
//! Tool activity lives in assistant `toolCall` content blocks, paired to their
//! `toolResult` line by `toolCallId`. pi's own per-token cost is ignored — the
//! server prices from token counts like every other agent.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufReader;
use std::sync::OnceLock;

use modelstat_redact::redact;
use modelstat_wire::{
    source_event_id, tc_fallback_id, EventSource, GitContext, RawEvent, TokenUsage,
};
use regex::Regex;
use serde_json::Value;

use crate::git::guess_repo_slug_from_path;
use crate::line_reader::OffsetLines;
use crate::references::detect_event_references;
use crate::tool_action::{extract_local_tool_context, extract_tool_action, ToolActionInput};
use crate::tool_hash::{hash_args, json_bytes, split_observed_tool_name, tool_identity};
use crate::types::{LocalToolContext, ParseResult, ParseStats, ParserContext, Sink, ToolCallDraft};
use crate::util::{collapse_ws, slice_utf16, strip_code};

/// Map pi's free-form provider string onto the closed PROVIDERS enum. Substring
/// matches (not equality) — pi sometimes records a model name in the provider slot.
fn map_provider(raw: Option<&str>) -> &'static str {
    let p = match raw {
        Some(s) if !s.is_empty() => s.to_lowercase(),
        _ => return "unknown",
    };
    let has = |needle: &str| p.contains(needle);
    if has("anthropic") || has("claude") {
        "anthropic"
    } else if has("openai") || has("gpt") || has("codex") {
        "openai"
    } else if has("google") || has("gemini") {
        "google"
    } else if has("deepseek") {
        "deepseek"
    } else if has("moonshot") || has("kimi") {
        "moonshot"
    } else if has("mistral") {
        "mistral"
    } else if has("xai") || has("grok") {
        "xai"
    } else if has("ollama") {
        "ollama_local"
    } else {
        "unknown"
    }
}

/// pi session filename is `<ISO-ish-TS>_<session-uuid>.jsonl`.
pub fn derive_session_id_from_pi_path(path: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"_([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$")
            .unwrap()
    });
    re.captures(path)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Join every `text` block's text (thinking/toolCall blocks dropped).
fn join_text_blocks(content: &Value) -> String {
    match content {
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            parts.join(" ")
        }
        _ => String::new(),
    }
}

fn extract_excerpt(content: &Value) -> Option<String> {
    let text = join_text_blocks(content);
    if text.is_empty() {
        return None;
    }
    let text = collapse_ws(&strip_code(&text));
    if text.is_empty() {
        return None;
    }
    let cleaned = redact(&text, None).text;
    let truncated = slice_utf16(&cleaned, 320);
    if truncated.is_empty() {
        None
    } else {
        Some(truncated)
    }
}

fn collect_ref_text(content: &Value) -> String {
    slice_utf16(&join_text_blocks(content), 64_000)
}

pub fn parse_pi_session(ctx: &ParserContext) -> std::io::Result<ParseResult> {
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

pub fn parse_pi_session_streaming(
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
    let mut pending_by_call_id: HashMap<String, usize> = HashMap::new();

    let mut raw_lines: u64 = 0;
    let mut emitted: u64 = 0;
    let mut skipped: u64 = 0;

    let file = File::open(&ctx.source_file)?;
    let mut lines = OffsetLines::new(BufReader::new(file), ctx.byte_offset_start);

    let mut session_id: Option<String> = derive_session_id_from_pi_path(&ctx.source_file);
    let mut cwd: Option<String> = None;
    let mut last_provider: Option<String> = None;
    let mut last_model: Option<String> = None;

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
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");

        if kind == "session" {
            if session_id.is_none() {
                if let Some(id) = obj.get("id").and_then(Value::as_str) {
                    session_id = Some(id.to_string());
                }
            }
            if let Some(c) = obj.get("cwd").and_then(Value::as_str) {
                cwd = Some(c.to_string());
            }
            continue;
        }

        if kind == "model_change" {
            if let Some(pv) = obj.get("provider").and_then(Value::as_str) {
                last_provider = Some(pv.to_string());
            }
            if let Some(mi) = obj.get("modelId").and_then(Value::as_str) {
                last_model = Some(mi.to_string());
            }
            continue;
        }

        if kind != "message" {
            skipped += 1;
            continue;
        }

        let message = obj.get("message").cloned().unwrap_or(Value::Null);
        let role = message.get("role").and_then(Value::as_str);
        let ml_timestamp = obj.get("timestamp").and_then(Value::as_str);
        if role.is_none() || ml_timestamp.is_none() || session_id.is_none() {
            skipped += 1;
            continue;
        }
        let role = role.unwrap();
        let ml_timestamp = ml_timestamp.unwrap();
        let content = message.get("content").cloned().unwrap_or(Value::Null);

        if role == "assistant" {
            if let Some(m) = message.get("model").and_then(Value::as_str) {
                last_model = Some(m.to_string());
            }
            if let Some(p) = message.get("provider").and_then(Value::as_str) {
                last_provider = Some(p.to_string());
            }
            let provider = map_provider(
                message
                    .get("provider")
                    .and_then(Value::as_str)
                    .or(last_provider.as_deref()),
            );
            let model = message
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| last_model.clone());
            let event_id = source_event_id(
                &ctx.device_id,
                &EventSource::File {
                    file: &ctx.source_file,
                    byte_offset: offset,
                },
            );
            let slug = guess_repo_slug_from_path(cwd.as_deref());
            let excerpt = extract_excerpt(&content);
            let refs = detect_event_references(&collect_ref_text(&content));

            let mut aggregate: BTreeMap<String, u64> = BTreeMap::new();
            if let Value::Array(blocks) = &content {
                let mut call_index: u64 = 0;
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                        continue;
                    }
                    let index = call_index;
                    call_index += 1;
                    let observed = block
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or("");
                    if observed.is_empty() {
                        continue;
                    }
                    let (server, name) = split_observed_tool_name(observed);
                    let input = block.get("arguments").cloned().unwrap_or(Value::Null);
                    let hashes = hash_args(&input);
                    let raw_id = block.get("id").and_then(Value::as_str);
                    let external_call_id = match raw_id {
                        Some(s) if !s.trim().is_empty() => slice_utf16(s.trim(), 120),
                        _ => tc_fallback_id(&event_id, index),
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
                        session_id: session_id.clone().unwrap(),
                        source_event_id: event_id.clone(),
                        agent: "pi".to_string(),
                        server: server.clone(),
                        name: name.clone(),
                        turn_index: None,
                        call_index: index,
                        started_at: ml_timestamp.to_string(),
                        ended_at: None,
                        status: "unknown".to_string(),
                        args_hash: hashes.args_hash,
                        signature_hash: hashes.signature_hash,
                        args_bytes: hashes.args_bytes,
                        result_bytes: 0,
                        model: model.clone(),
                        action: Some(action),
                    };
                    let identity = tool_identity(&server, &name);
                    *aggregate.entry(identity).or_insert(0) += 1;
                    tool_calls.push(draft);
                    if let Some(id) = raw_id {
                        if !id.is_empty() {
                            pending_by_call_id.insert(id.to_string(), tool_calls.len() - 1);
                        }
                    }
                }
            }

            let usage = message.get("usage").cloned().unwrap_or(Value::Null);
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
                source_event_id: event_id,
                ts: ml_timestamp.to_string(),
                kind: "assistant_message".to_string(),
                agent: "pi".to_string(),
                provider: provider.to_string(),
                model,
                session_id: session_id.clone().unwrap(),
                turn_index: None,
                parent_event_id: None,
                cwd: cwd.clone(),
                git,
                tokens: Some(TokenUsage {
                    input: usage.get("input").and_then(Value::as_u64).unwrap_or(0),
                    output: usage.get("output").and_then(Value::as_u64).unwrap_or(0),
                    cache_creation: usage.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0),
                    cache_read: usage.get("cacheRead").and_then(Value::as_u64).unwrap_or(0),
                    reasoning: 0,
                }),
                duration_ms: None,
                tool_calls: aggregate,
                files_touched: Vec::new(),
                content_excerpt: excerpt,
                references: refs,
                source_file: Some(ctx.source_file.clone()),
                source_byte_offset: Some(offset),
                pricing_mode: None,
            });
            emitted += 1;
            continue;
        }

        if role == "toolResult" {
            if let Some(ref_id) = message.get("toolCallId").and_then(Value::as_str) {
                if let Some(idx) = pending_by_call_id.remove(ref_id) {
                    let is_error = message.get("isError").and_then(Value::as_bool) == Some(true);
                    let result = message.get("content").cloned().unwrap_or(Value::Null);
                    let draft = &mut tool_calls[idx];
                    draft.ended_at = Some(ml_timestamp.to_string());
                    draft.status = if is_error { "error" } else { "success" }.to_string();
                    draft.result_bytes = json_bytes(&result);
                }
            }
            skipped += 1;
            continue;
        }

        // user message
        let excerpt = extract_excerpt(&content);
        let refs = detect_event_references(&collect_ref_text(&content));
        sink.push(RawEvent {
            source_event_id: source_event_id(
                &ctx.device_id,
                &EventSource::File {
                    file: &ctx.source_file,
                    byte_offset: offset,
                },
            ),
            ts: ml_timestamp.to_string(),
            kind: "user_message".to_string(),
            agent: "pi".to_string(),
            provider: map_provider(last_provider.as_deref()).to_string(),
            model: last_model.clone(),
            session_id: session_id.clone().unwrap(),
            turn_index: None,
            parent_event_id: None,
            cwd: cwd.clone(),
            git: None,
            tokens: None,
            duration_ms: None,
            tool_calls: BTreeMap::new(),
            files_touched: Vec::new(),
            content_excerpt: excerpt,
            references: refs,
            source_file: Some(ctx.source_file.clone()),
            source_byte_offset: Some(offset),
            pricing_mode: None,
        });
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
    ))
}
