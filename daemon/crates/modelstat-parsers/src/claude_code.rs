//! Claude Code JSONL parser — a byte-for-byte port of
//! `packages/parsers/src/claude-code/index.ts`.
//!
//! Resume-copy dedupe (the module's core subtlety): `claude --resume` writes a
//! NEW `<new-uuid>.jsonl` beginning with byte-identical copies of the ancestor
//! session's lines (each keeping its original `sessionId`/`uuid`). A line whose
//! `sessionId` ≠ the filename uuid is a resume copy — dropped when the ancestor's
//! own `<sid>.jsonl` still exists anywhere under the projects root (else emitted
//! once, keyed by line uuid, so orphaned history survives exactly once).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::OnceLock;

use modelstat_redact::redact;
use modelstat_wire::{source_event_id, EventSource, RawEvent, TokenUsage};
use regex::Regex;
use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::git::{guess_repo_slug_from_path, path_guessed_git_context};
use crate::line_reader::OffsetLines;
use crate::references::detect_event_references;
use crate::skips::{unknown_record_event, SkipLedger, UnknownRecord};
use crate::tool_action::{extract_local_tool_context, extract_tool_action, ToolActionInput};
use crate::tool_hash::{hash_args, json_bytes, split_observed_tool_name, tool_identity};
use crate::types::{LocalToolContext, ParseResult, ParseStats, ParserContext, Sink, ToolCallDraft};
use crate::util::{slice_utf16, stated_duration_ms};
use modelstat_wire::tc_fallback_id;

/// The instant this record states about itself, or None when it states none.
///
/// A message without an instant cannot be placed in a conversation — and every
/// wait the server derives is the distance between two of these, so an empty
/// string is not a missing field but a wrong answer: it parses as epoch 0 and
/// puts the message in 1970. The unknown-record path has always required a
/// stated instant; the modelled arms require it too.
fn stated_ts(obj: &Value) -> Option<String> {
    obj.get("timestamp")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// `<session-uuid>.jsonl` at the end of a path → the uuid.
pub fn derive_session_id_from_filename(path: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$")
            .unwrap()
    });
    re.captures(path)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Claude encodes `/` as `-`. Not a perfect inverse; good enough for display.
pub fn decode_encoded_dir(encoded: &str) -> String {
    let stripped = encoded
        .strip_prefix('-')
        .map(|s| format!("/{s}"))
        .unwrap_or_else(|| encoded.to_string());
    stripped.replace('-', "/")
}

/// Extract the message's redacted VERBATIM text (SPEC 0005) plus its length in
/// chars. Only `text` blocks contribute, and NOTHING is processed away — no
/// code stripping, no paste heuristics, no truncation: the world is too
/// diverse for regex/thresholds, so any semantic judgment about the text
/// (typed vs pasted, noise vs signal) belongs to the LLM layers downstream.
/// The only bound anywhere is the wire clamp
/// ([`modelstat_wire::caps::CONTENT_EXCERPT_MAX`]), an extreme
/// malicious-size guard, and redaction — the deterministic security floor —
/// still runs before anything leaves the machine.
fn extract_excerpt(content: &Value) -> Option<(String, u64)> {
    let text = join_text(content)?;
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

/// Join a message's natural-language TEXT (string content or `text` blocks). For
/// the excerpt + reference passes. Returns None when there's no text at all.
fn join_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            let joined = parts.join("\n\n");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

/// Full-length text for public-reference detection (capped at 64k). Not redacted.
fn collect_ref_text(content: &Value) -> String {
    let text = join_text(content).unwrap_or_default();
    slice_utf16(&text, 64_000)
}

struct AncestorCache {
    source_dir: std::path::PathBuf,
    cache: HashMap<String, bool>,
}

impl AncestorCache {
    fn new(source_file: &str) -> Self {
        Self {
            source_dir: Path::new(source_file)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            cache: HashMap::new(),
        }
    }

    /// Is the ancestor session's own transcript still on disk? Fast same-dir
    /// probe, then a sibling-dir walk under the projects root. Memoised.
    fn exists(&mut self, sid: &str) -> bool {
        if let Some(&v) = self.cache.get(sid) {
            return v;
        }
        let mut found = false;
        let same = self.source_dir.join(format!("{sid}.jsonl"));
        if same.exists() {
            found = true;
        } else if let Some(root) = self.source_dir.parent() {
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                        && root
                            .join(entry.file_name())
                            .join(format!("{sid}.jsonl"))
                            .exists()
                    {
                        found = true;
                        break;
                    }
                }
            }
        }
        self.cache.insert(sid.to_string(), found);
        found
    }
}

/// Parse a Claude Code transcript, collecting events.
pub fn parse_claude_code_jsonl(ctx: &ParserContext) -> std::io::Result<ParseResult> {
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

/// Parse a Claude Code transcript, streaming events to `emit` in bounded chunks.
pub fn parse_claude_code_jsonl_streaming(
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

    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut last_model: Option<String> = None;
    // Conversation turn ordinal (SPEC 0005): a turn starts at each REAL user
    // prompt (typed text — tool-result-only and injected-noise-only user lines
    // don't start one). Assistant events and tool drafts inherit the current
    // ordinal; events before the first prompt sit in turn 0.
    let mut current_turn: u64 = 0;
    let mut saw_user_prompt = false;
    // The agent to stamp: the host's label (Claude Desktop runs Claude Code in
    // this exact format) or the parser's own name. Resolved once — it is a
    // property of the job, not of a line.
    let agent_name = ctx.agent("claude_code");

    let filename_session_id = derive_session_id_from_filename(&ctx.source_file);
    let mut ancestors = AncestorCache::new(&ctx.source_file);

    // Dedupe id for the line about to be emitted, or None to drop it (a resume
    // copy whose ancestor transcript is still on disk).
    let dedupe_id_for = |session_id: &Option<String>,
                         line_uuid: &str,
                         byte_offset: u64,
                         ancestors: &mut AncestorCache|
     -> Option<String> {
        let is_resume_copy = match (&filename_session_id, session_id) {
            (Some(f), Some(s)) => s != f,
            _ => false,
        };
        if !is_resume_copy {
            return Some(source_event_id(
                &ctx.device_id,
                &EventSource::File {
                    file: &ctx.source_file,
                    byte_offset,
                },
            ));
        }
        let sid = session_id.as_deref().unwrap_or("");
        if ancestors.exists(sid) {
            None // drop
        } else {
            Some(source_event_id(
                &ctx.device_id,
                &EventSource::LineUuid { line_uuid },
            ))
        }
    };

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

        // Modelled and declined on purpose: a queue operation is the CLI's own
        // bookkeeping, not a turn. A DECISION, not a failure to read, so it stays
        // out of the skip ledger — see `crate::skips`.
        if kind == "queue-operation" {
            skipped += 1;
            continue;
        }

        if kind == "user" || kind == "assistant" {
            if let Some(s) = obj.get("sessionId").and_then(Value::as_str) {
                session_id = Some(s.to_string());
            }
            if let Some(c) = obj.get("cwd").and_then(Value::as_str) {
                cwd = Some(c.to_string());
            }
            if let Some(b) = obj.get("gitBranch").and_then(Value::as_str) {
                git_branch = Some(b.to_string());
            }
        }

        if kind == "assistant" {
            let message = obj.get("message").cloned().unwrap_or(Value::Null);
            let usage = message.get("usage").cloned().unwrap_or(Value::Null);
            let model = message
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(m) = &model {
                if m != "<synthetic>" {
                    last_model = Some(m.clone());
                }
            }
            let uuid = obj.get("uuid").and_then(Value::as_str);
            let ts = stated_ts(&obj);
            if uuid.is_none() || ts.is_none() || session_id.is_none() {
                // A kind we DO model, arriving without the fields it is defined
                // by. Ledgered under its own name: rename `uuid` upstream and
                // every turn lands here, which is the same silent-zero-output
                // failure as an unknown kind wearing a familiar label.
                skipped += 1;
                skips.drop_record(&ctx.source_file, kind);
                continue;
            }
            let uuid = uuid.unwrap().to_string();
            let ts = ts.unwrap();
            let event_id = match dedupe_id_for(&session_id, &uuid, offset, &mut ancestors) {
                Some(id) => id,
                None => {
                    skipped += 1;
                    continue;
                }
            };

            let slug = guess_repo_slug_from_path(cwd.as_deref());
            let content = message.get("content").cloned().unwrap_or(Value::Null);
            let (excerpt, content_bytes) = match extract_excerpt(&content) {
                Some((text, chars)) => (Some(text), Some(chars)),
                None => (None, None),
            };
            let refs = detect_event_references(&collect_ref_text(&content));

            let mut aggregate: std::collections::BTreeMap<String, u64> =
                std::collections::BTreeMap::new();
            if let Value::Array(blocks) = &content {
                let mut call_index: u64 = 0;
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_use") {
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
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    let draft = build_tool_call_draft(
                        observed,
                        block.get("id"),
                        &input,
                        session_id.as_deref().unwrap(),
                        &event_id,
                        index,
                        &ts,
                        model.as_deref(),
                        cwd.as_deref(),
                        Some(current_turn),
                        &agent_name,
                        &mut script_contexts,
                    );
                    let identity = tool_identity(&draft.server, &draft.name);
                    *aggregate.entry(identity).or_insert(0) += 1;
                    let call_id_opt = block.get("id").and_then(Value::as_str).map(str::to_string);
                    tool_calls.push(draft);
                    if let Some(id) = call_id_opt {
                        if !id.is_empty() {
                            pending_by_call_id.insert(id, tool_calls.len() - 1);
                        }
                    }
                }
            }

            let git = path_guessed_git_context(slug.clone(), git_branch.clone());

            sink.push(RawEvent {
                source_event_id: event_id,
                ts,
                kind: "assistant_message".to_string(),
                agent: agent_name.clone(),
                provider: "anthropic".to_string(),
                model,
                session_id: session_id.clone().unwrap(),
                turn_index: Some(current_turn),
                parent_event_id: obj
                    .get("parentUuid")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                cwd: cwd.clone(),
                git,
                tokens: Some(TokenUsage {
                    input: usage_u64(&usage, "input_tokens"),
                    output: usage_u64(&usage, "output_tokens"),
                    cache_creation: usage_u64(&usage, "cache_creation_input_tokens"),
                    cache_read: usage_u64(&usage, "cache_read_input_tokens"),
                    reasoning: 0,
                }),
                tokens_unmapped: std::collections::BTreeMap::new(),
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
        } else if kind == "user" {
            let message = obj.get("message").cloned().unwrap_or(Value::Null);
            let content = message.get("content").cloned().unwrap_or(Value::Null);

            // Pair tool_result blocks back to their pending drafts (by tool_use_id).
            if let Value::Array(blocks) = &content {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let ref_id = match block.get("tool_use_id").and_then(Value::as_str) {
                        Some(r) => r,
                        None => continue,
                    };
                    if let Some(idx) = pending_by_call_id.remove(ref_id) {
                        let ts = obj.get("timestamp").and_then(Value::as_str).unwrap_or("");
                        let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
                        let result = block.get("content").cloned().unwrap_or(Value::Null);
                        let draft = &mut tool_calls[idx];
                        draft.ended_at = Some(ts.to_string());
                        draft.status = if is_error { "error" } else { "success" }.to_string();
                        draft.result_bytes = json_bytes(&result);
                    }
                }
            }

            let uuid = obj.get("uuid").and_then(Value::as_str);
            let ts = stated_ts(&obj);
            if uuid.is_none() || ts.is_none() || session_id.is_none() {
                // A kind we DO model, arriving without the fields it is defined
                // by. Ledgered under its own name: rename `uuid` upstream and
                // every turn lands here, which is the same silent-zero-output
                // failure as an unknown kind wearing a familiar label.
                skipped += 1;
                skips.drop_record(&ctx.source_file, kind);
                continue;
            }
            let uuid = uuid.unwrap().to_string();
            let ts = ts.unwrap();
            let event_id = match dedupe_id_for(&session_id, &uuid, offset, &mut ancestors) {
                Some(id) => id,
                None => {
                    skipped += 1;
                    continue;
                }
            };
            let (excerpt, content_bytes) = match extract_excerpt(&content) {
                Some((text, chars)) => (Some(text), Some(chars)),
                None => (None, None),
            };
            // A REAL prompt (typed text survived the injected-noise strip)
            // starts a new turn; tool-result-only and noise-only user lines
            // stay in the current one.
            if excerpt.is_some() {
                if saw_user_prompt {
                    current_turn += 1;
                }
                saw_user_prompt = true;
            }
            // Claude Code stamps its own measured duration on the line that
            // carries a tool result — ship it as stated, never derived
            // (SPEC 0005). Which FIELD it lands in is the tool's choice, not the
            // format's (`durationMs`, `durationSeconds`, `totalDurationMs` all
            // ship in one release), so the unit is read off the name rather than
            // assumed — see `stated_duration_ms`. Ambiguous multi-result lines
            // share one number; the per-call truth stays ToolCallWire's
            // started/ended pair.
            let duration_ms = obj.get("toolUseResult").and_then(stated_duration_ms);
            let refs = detect_event_references(&collect_ref_text(&content));
            sink.push(RawEvent {
                source_event_id: event_id,
                ts,
                kind: "user_message".to_string(),
                agent: ctx.agent("claude_code"),
                provider: "anthropic".to_string(),
                model: last_model.clone(),
                session_id: session_id.clone().unwrap(),
                turn_index: Some(current_turn),
                parent_event_id: obj
                    .get("parentUuid")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                cwd: cwd.clone(),
                git: None,
                tokens: None,
                tokens_unmapped: std::collections::BTreeMap::new(),
                duration_ms,
                tool_calls: std::collections::BTreeMap::new(),
                files_touched: Vec::new(),
                content_excerpt: excerpt,
                content_bytes,
                references: refs,
                source_file: Some(ctx.source_file.clone()),
                source_byte_offset: Some(offset),
                redactions: Default::default(),
            });
            emitted += 1;
        } else if kind == "tool_use" {
            // Top-level `type:'tool_use'` line — a per-call draft only (never an
            // event), anchored to this line's own byte offset.
            let observed = obj
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            let ts = obj.get("timestamp").and_then(Value::as_str);
            let sid = obj
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| session_id.clone());
            if observed.is_empty() || ts.is_none() || sid.is_none() {
                skipped += 1;
                continue;
            }
            let src_id = source_event_id(
                &ctx.device_id,
                &EventSource::File {
                    file: &ctx.source_file,
                    byte_offset: offset,
                },
            );
            let input = obj.get("input").cloned().unwrap_or(Value::Null);
            let draft = build_tool_call_draft(
                observed,
                obj.get("id"),
                &input,
                sid.as_deref().unwrap(),
                &src_id,
                0,
                ts.unwrap(),
                last_model.as_deref(),
                cwd.as_deref(),
                Some(current_turn),
                &agent_name,
                &mut script_contexts,
            );
            let call_id_opt = obj.get("id").and_then(Value::as_str).map(str::to_string);
            tool_calls.push(draft);
            if let Some(id) = call_id_opt {
                if !id.is_empty() {
                    pending_by_call_id.insert(id, tool_calls.len() - 1);
                }
            }
        } else {
            // A `type` no arm above matches. Counted AND surfaced as a minimal
            // event, so the record is visible server-side instead of vanishing
            // the way Cursor's moved schema did. Content stays behind — see
            // `skips::UnknownRecord`.
            skips.drop_record(&ctx.source_file, kind);
            // An event needs a session and an instant, and both must be STATED
            // rather than assumed: a record that dates itself can be placed in
            // the conversation, and one that does not cannot. Claude Desktop's
            // `ai-title` / `last-prompt` / `mode` lines carry no timestamp and so
            // report only through the ledger — which is the gate working, not a
            // gap in it.
            let sid = session_id.clone().or_else(|| filename_session_id.clone());
            let ts = obj.get("timestamp").and_then(Value::as_str);
            // Resume copies duplicate every line of their ancestor, unknown ones
            // included, so they take the same dedupe as a turn: keyed by line
            // uuid when the ancestor is gone, dropped when it is still on disk.
            // Without this an `attachment` would double on every `--resume`.
            let event_id = match obj.get("uuid").and_then(Value::as_str) {
                Some(uuid) => dedupe_id_for(&session_id, uuid, offset, &mut ancestors),
                None => Some(source_event_id(
                    &ctx.device_id,
                    &EventSource::File {
                        file: &ctx.source_file,
                        byte_offset: offset,
                    },
                )),
            };
            match (sid, ts, event_id) {
                (Some(sid), Some(ts), Some(event_id)) => {
                    sink.push(unknown_record_event(UnknownRecord {
                        kind,
                        source_event_id: event_id,
                        agent: &agent_name,
                        provider: "anthropic",
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

#[allow(clippy::too_many_arguments)]
fn build_tool_call_draft(
    observed_name: &str,
    raw_call_id: Option<&Value>,
    input: &Value,
    session_id: &str,
    source_event_id_str: &str,
    call_index: u64,
    started_at: &str,
    model: Option<&str>,
    cwd: Option<&str>,
    turn_index: Option<u64>,
    agent: &str,
    contexts: &mut Vec<LocalToolContext>,
) -> ToolCallDraft {
    let (server, name) = split_observed_tool_name(observed_name);
    let hashes = hash_args(input);
    let external_call_id = match raw_call_id.and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => slice_utf16(s.trim(), 120),
        _ => tc_fallback_id(source_event_id_str, call_index),
    };
    if let Some((command, ctx_cwd)) = extract_local_tool_context(&ToolActionInput {
        server: &server,
        name: &name,
        input,
        cwd,
    }) {
        contexts.push(LocalToolContext {
            external_call_id: external_call_id.clone(),
            command,
            cwd: ctx_cwd,
        });
    }
    let action = extract_tool_action(&ToolActionInput {
        server: &server,
        name: &name,
        input,
        cwd,
    });
    ToolCallDraft {
        external_call_id,
        session_id: session_id.to_string(),
        source_event_id: source_event_id_str.to_string(),
        agent: agent.to_string(),
        server,
        name,
        turn_index,
        call_index,
        started_at: started_at.to_string(),
        ended_at: None,
        status: "unknown".to_string(),
        args_hash: hashes.args_hash,
        signature_hash: hashes.signature_hash,
        args_bytes: hashes.args_bytes,
        result_bytes: 0,
        model: model.map(str::to_string),
        action: Some(action),
    }
}

fn usage_u64(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Quick checksum of a JSONL file (size + last-line tail) for the discovery
/// layer's re-parse decision. Port of TS `quickChecksum`.
#[derive(Debug, Clone, PartialEq)]
pub struct QuickChecksum {
    pub size: u64,
    /// Modification time in milliseconds since the Unix epoch (matches JS
    /// `stat.mtimeMs`).
    pub mtime: f64,
    pub tail_hash: String,
}

pub fn quick_checksum(path: &str) -> std::io::Result<QuickChecksum> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let mut file = File::open(path)?;
    let start = size.saturating_sub(4096);
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let digest = Sha1::digest(&buf);
    let mut hex = String::with_capacity(40);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(QuickChecksum {
        size,
        mtime,
        tail_hash: hex[..16].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A transcript of `lines`, at a path whose uuid is the session's — so the
    /// resume-copy rule sees a native file. Shapes only: every value below is
    /// fabricated to match the FORM Claude Code writes, never copied from one.
    fn transcript(lines: &[Value]) -> (String, tempdir::Guard) {
        let dir = std::env::temp_dir().join(format!(
            "modelstat-cc-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb.jsonl");
        let text: String = lines
            .iter()
            .map(|l| format!("{}\n", serde_json::to_string(l).unwrap()))
            .collect();
        std::fs::write(&path, text).unwrap();
        (
            path.to_string_lossy().into_owned(),
            tempdir::Guard(dir.to_string_lossy().into_owned()),
        )
    }

    mod tempdir {
        pub struct Guard(pub String);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn user_line(uuid: &str, text: &str) -> Value {
        serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb",
            "timestamp": "2026-06-01T10:00:00.000Z",
            "cwd": "/Users/dev/Projects/acme",
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        })
    }

    /// Claude Code states its own elapsed time under the name the TOOL chose,
    /// with the unit in that name — three spellings ship in one release. The
    /// parser used to read exactly one of them, so a web search and a sub-agent
    /// run (the two longest tool calls a session makes) reported no duration at
    /// all, and no reader could recover the number: it exists only in the local
    /// JSONL.
    #[test]
    fn every_spelling_of_claude_s_own_measured_duration_ships() {
        let with_result = |uuid: &str, result: Value| {
            serde_json::json!({
                "type": "user",
                "uuid": uuid,
                "sessionId": "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb",
                "timestamp": "2026-06-01T10:00:01.000Z",
                "toolUseResult": result,
                "message": { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_0123", "content": "ok" }
                ]},
            })
        };
        let (path, _guard) = transcript(&[
            user_line("u-1", "go"),
            with_result(
                "u-2",
                serde_json::json!({ "url": "https://example.test", "durationMs": 3500 }),
            ),
            with_result(
                "u-3",
                serde_json::json!({ "query": "acme", "durationSeconds": 7.5 }),
            ),
            with_result(
                "u-4",
                serde_json::json!({ "agentType": "general", "totalDurationMs": 105_000 }),
            ),
            with_result(
                "u-5",
                serde_json::json!({ "stdout": "", "timeoutMs": 120_000 }),
            ),
        ]);
        let res = parse_claude_code_jsonl(&ParserContext::new("dev_1", path)).unwrap();
        let durations: Vec<Option<u64>> = res.events.iter().map(|e| e.duration_ms).collect();
        assert_eq!(
            durations,
            vec![None, Some(3500), Some(7500), Some(105_000), None],
            "each tool's own spelling, read in the unit it states; a configured \
             timeout is not an elapsed time"
        );
    }

    /// Every wait the server derives is the distance between two message
    /// timestamps, so a message with no stated instant is not a message with a
    /// missing field — it is one that lands in 1970 and drags a whole session's
    /// timing with it. The line is counted under its own kind instead, exactly
    /// as a line missing its `uuid` already was.
    #[test]
    fn a_turn_that_states_no_instant_is_ledgered_rather_than_dated_to_the_epoch() {
        let undated = |uuid: &str, kind: &str| {
            serde_json::json!({
                "type": kind,
                "uuid": uuid,
                "sessionId": "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb",
                "message": { "role": kind, "content": [{ "type": "text", "text": "hello" }] },
            })
        };
        let (path, _guard) = transcript(&[
            user_line("u-1", "the dated prompt"),
            undated("u-2", "user"),
            undated("a-1", "assistant"),
        ]);
        let res = parse_claude_code_jsonl(&ParserContext::new("dev_1", path)).unwrap();
        assert_eq!(res.events.len(), 1, "only the dated line becomes an event");
        assert_eq!(res.events[0].ts, "2026-06-01T10:00:00.000Z");
        assert!(
            res.events.iter().all(|e| !e.ts.is_empty()),
            "no event may carry an empty instant"
        );
        assert_eq!(res.skipped_kinds.get("user"), Some(&1));
        assert_eq!(res.skipped_kinds.get("assistant"), Some(&1));
    }

    /// A tool call's own start is the instant the assistant line states, and it
    /// is never blank — the same gate the event takes.
    #[test]
    fn a_tool_call_starts_at_the_instant_its_line_states() {
        let (path, _guard) = transcript(&[
            user_line("u-1", "read the file"),
            serde_json::json!({
                "type": "assistant",
                "uuid": "a-1",
                "sessionId": "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb",
                "timestamp": "2026-06-01T10:00:02.000Z",
                "message": { "model": "claude-opus-4-8", "content": [
                    { "type": "tool_use", "id": "toolu_0123", "name": "Read",
                      "input": { "file_path": "/Users/dev/Projects/acme/x.ts" } }
                ]},
            }),
            serde_json::json!({
                "type": "user",
                "uuid": "u-2",
                "sessionId": "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb",
                "timestamp": "2026-06-01T10:00:03.500Z",
                "message": { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_0123", "content": "ok" }
                ]},
            }),
        ]);
        let res = parse_claude_code_jsonl(&ParserContext::new("dev_1", path)).unwrap();
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].started_at, "2026-06-01T10:00:02.000Z");
        assert_eq!(
            res.tool_calls[0].ended_at.as_deref(),
            Some("2026-06-01T10:00:03.500Z")
        );
    }
}
