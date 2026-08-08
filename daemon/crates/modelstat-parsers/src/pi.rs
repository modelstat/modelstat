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

/// The provider string the transcript states, lowercased, or `"unknown"` when it
/// states none.
///
/// pi is a multi-vendor harness: the set of provider names it can write is
/// whatever its config file lists, which is open and grows without us. This used
/// to run through a fixed table that folded every unlisted vendor to `"unknown"`
/// — so a pi user on zhipu had every event stamped `unknown`, while the identity
/// probe that reads the very same config emitted the key under `zhipu`, and the
/// join that pairs a session with the account that paid for it could never
/// match. A closed table over an open set does not merely lose detail here; it
/// severs the two halves of the record from each other.
///
/// Lowercasing is the one normalisation kept, and it is forced: the identity
/// probe lowercases the config's provider keys, and the two strings have to be
/// comparable. `"unknown"` survives for the genuinely-absent case only — the
/// wire's `provider` is required, so silence needs a word, and that word is not
/// a claim about any vendor.
fn provider_of(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .unwrap_or_else(|| "unknown".to_string())
}

/// One assistant message's token counters, or an honest statement that they
/// could not be read.
///
/// Three cases, and only the first two were previously distinguishable:
///
///   * no `usage` object at all → `(None, empty)`. The message states no usage,
///     so the event states none either. It used to ship `{0,0,0,0,0}`, which is
///     a claim — that this round trip cost nothing — and it is indistinguishable
///     from a real zero. That fabricated zero is the v17 bug, and it cost a full
///     fleet re-scan to undo.
///   * all four counters present and numeric → `(Some, empty)`, the observed case.
///   * a counter missing or non-numeric → `(None, numeric leaves)`. The wire's
///     five buckets always materialise, so there is no way to say "this one
///     field is unknown" inside them; the whole reading is withheld and every
///     number `usage` did state rides `tokens_unmapped` instead, where it can be
///     re-bucketed later rather than being lost behind a zero.
///
/// `totalTokens` is deliberately never mapped: pi's total is INCLUSIVE of the
/// other counters and our buckets are disjoint, so adding it would double-bill.
fn pi_token_usage(usage: &Value) -> (Option<TokenUsage>, BTreeMap<String, u64>) {
    if usage.is_null() {
        return (None, BTreeMap::new());
    }
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    let (Some(input), Some(output), Some(cache_creation), Some(cache_read)) = (
        field("input"),
        field("output"),
        field("cacheWrite"),
        field("cacheRead"),
    ) else {
        return (None, numeric_leaves(usage));
    };
    (
        Some(TokenUsage {
            input,
            output,
            cache_creation,
            cache_read,
            // pi records no separate reasoning counter; 0 is the true value here,
            // not a default for something it stated and we failed to read.
            reasoning: 0,
        }),
        BTreeMap::new(),
    )
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
            parts.join("\n\n")
        }
        _ => String::new(),
    }
}

/// SPEC 0005: redacted VERBATIM text + its length in chars (mirrors
/// `claude_code::extract_excerpt` — nothing processed away, no truncation;
/// the wire clamp is the only, extreme, bound).
fn extract_excerpt(content: &Value) -> Option<(String, u64)> {
    let text = join_text_blocks(content);
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

fn collect_ref_text(content: &Value) -> String {
    slice_utf16(&join_text_blocks(content), 64_000)
}

pub fn parse_pi_session(ctx: &ParserContext) -> std::io::Result<ParseResult> {
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

pub fn parse_pi_session_streaming(
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
    let mut pending_by_call_id: HashMap<String, usize> = HashMap::new();

    let mut raw_lines: u64 = 0;
    let mut emitted: u64 = 0;
    let mut skipped: u64 = 0;
    let mut skips = SkipLedger::default();

    let file = File::open(&ctx.source_file)?;
    let mut lines = OffsetLines::new(BufReader::new(file), ctx.byte_offset_start);

    let mut session_id: Option<String> = derive_session_id_from_pi_path(&ctx.source_file);
    let mut cwd: Option<String> = None;
    let mut last_provider: Option<String> = None;
    let mut last_model: Option<String> = None;
    // Conversation turn ordinal (SPEC 0005) — see the claude_code parser.
    let mut current_turn: u64 = 0;
    let mut saw_user_prompt = false;

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

        // A top-level `type` nothing here models — `session` and `model_change`
        // are handled above and `message` below.
        if kind != "message" {
            skips.drop_record(&ctx.source_file, kind);
            match (
                session_id.clone(),
                obj.get("timestamp").and_then(Value::as_str),
            ) {
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
                        agent: "pi",
                        provider: &provider_of(last_provider.as_deref()),
                        session_id: sid,
                        ts: ts.to_string(),
                        turn_index: Some(current_turn),
                        duration_ms: stated_duration_ms(&obj),
                        source_file: &ctx.source_file,
                        source_byte_offset: Some(offset),
                    }));
                    emitted += 1;
                }
                _ => skipped += 1,
            }
            continue;
        }

        let message = obj.get("message").cloned().unwrap_or(Value::Null);
        let role = message.get("role").and_then(Value::as_str);
        let ml_timestamp = obj.get("timestamp").and_then(Value::as_str);
        if role.is_none() || ml_timestamp.is_none() || session_id.is_none() {
            // A kind we DO model, without the fields it is defined by — the same
            // silent failure as an unknown kind, wearing a familiar label.
            skipped += 1;
            skips.drop_record(&ctx.source_file, kind);
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
            let provider = provider_of(
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
            let (excerpt, content_bytes) = match extract_excerpt(&content) {
                Some((text, chars)) => (Some(text), Some(chars)),
                None => (None, None),
            };
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
                        turn_index: Some(current_turn),
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
            let (tokens, tokens_unmapped) = pi_token_usage(&usage);
            if tokens.is_none() && !tokens_unmapped.is_empty() {
                skips.drop_record(&ctx.source_file, "message/assistant/usage");
            }
            let git = path_guessed_git_context(slug.clone(), None);
            sink.push(RawEvent {
                source_event_id: event_id,
                ts: ml_timestamp.to_string(),
                kind: "assistant_message".to_string(),
                agent: "pi".to_string(),
                provider: provider.to_string(),
                model,
                session_id: session_id.clone().unwrap(),
                turn_index: Some(current_turn),
                parent_event_id: None,
                cwd: cwd.clone(),
                git,
                tokens,
                tokens_unmapped,
                duration_ms: None,
                tool_calls: aggregate,
                files_touched: Vec::new(),
                content_excerpt: excerpt,
                content_bytes,
                references: refs,
                source_file: Some(ctx.source_file.clone()),
                source_byte_offset: Some(offset),
                redactions: Default::default(),
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

        // A role nothing here models. It used to fall into the user-message path
        // below, which stamped `kind: "user_message"` on whatever it was — a
        // system or tool-status role would have been filed as something a human
        // typed, and nothing downstream could tell.
        if role != "user" {
            skips.drop_record(&ctx.source_file, &format!("message/{role}"));
            sink.push(unknown_record_event(UnknownRecord {
                kind: role,
                source_event_id: source_event_id(
                    &ctx.device_id,
                    &EventSource::File {
                        file: &ctx.source_file,
                        byte_offset: offset,
                    },
                ),
                agent: "pi",
                provider: &provider_of(last_provider.as_deref()),
                session_id: session_id.clone().unwrap(),
                ts: ml_timestamp.to_string(),
                turn_index: Some(current_turn),
                duration_ms: stated_duration_ms(&obj),
                source_file: &ctx.source_file,
                source_byte_offset: Some(offset),
            }));
            emitted += 1;
            continue;
        }

        let (excerpt, content_bytes) = match extract_excerpt(&content) {
            Some((text, chars)) => (Some(text), Some(chars)),
            None => (None, None),
        };
        // A real (typed) prompt starts a new turn — SPEC 0005, mirroring the
        // claude_code parser.
        if excerpt.is_some() {
            if saw_user_prompt {
                current_turn += 1;
            }
            saw_user_prompt = true;
        }
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
            provider: provider_of(last_provider.as_deref()),
            model: last_model.clone(),
            session_id: session_id.clone().unwrap(),
            turn_index: Some(current_turn),
            parent_event_id: None,
            cwd: cwd.clone(),
            git: None,
            tokens: None,
            tokens_unmapped: BTreeMap::new(),
            duration_ms: None,
            tool_calls: BTreeMap::new(),
            files_touched: Vec::new(),
            content_excerpt: excerpt,
            content_bytes,
            references: refs,
            source_file: Some(ctx.source_file.clone()),
            source_byte_offset: Some(offset),
            redactions: Default::default(),
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
        skips,
    ))
}
