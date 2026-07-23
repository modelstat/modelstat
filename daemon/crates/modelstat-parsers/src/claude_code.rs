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
use modelstat_wire::{source_event_id, EventSource, GitContext, RawEvent, TokenUsage};
use regex::Regex;
use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::git::guess_repo_slug_from_path;
use crate::line_reader::OffsetLines;
use crate::references::detect_event_references;
use crate::tool_action::{extract_local_tool_context, extract_tool_action, ToolActionInput};
use crate::tool_hash::{hash_args, json_bytes, split_observed_tool_name, tool_identity};
use crate::types::{LocalToolContext, ParseResult, ParseStats, ParserContext, Sink, ToolCallDraft};
use crate::util::{collapse_ws, slice_utf16, strip_code};
use modelstat_wire::tc_fallback_id;

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

/// Extract a redacted, ≤320-unit text excerpt from a message's content. Only
/// `text` blocks contribute; code fences are stripped, whitespace collapsed, and
/// the result is redacted before it leaves.
fn extract_excerpt(content: &Value) -> Option<String> {
    let text = join_text(content)?;
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
            let joined = parts.join(" ");
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

/// Parse a Claude Code transcript, streaming events to `emit` in bounded chunks.
pub fn parse_claude_code_jsonl_streaming(
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

    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut last_model: Option<String> = None;

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
            if uuid.is_none() || session_id.is_none() {
                skipped += 1;
                continue;
            }
            let uuid = uuid.unwrap().to_string();
            let event_id = match dedupe_id_for(&session_id, &uuid, offset, &mut ancestors) {
                Some(id) => id,
                None => {
                    skipped += 1;
                    continue;
                }
            };

            let slug = guess_repo_slug_from_path(cwd.as_deref());
            let content = message.get("content").cloned().unwrap_or(Value::Null);
            let excerpt = extract_excerpt(&content);
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
                        obj.get("timestamp").and_then(Value::as_str).unwrap_or(""),
                        model.as_deref(),
                        cwd.as_deref(),
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

            let git = if slug.is_some() || git_branch.is_some() {
                Some(GitContext {
                    remote_url: None,
                    remote_host: slug
                        .as_ref()
                        .filter(|s| s.contains('/'))
                        .map(|_| "github.com".to_string()),
                    remote_slug: slug.clone(),
                    branch: git_branch.clone(),
                })
            } else {
                None
            };

            sink.push(RawEvent {
                source_event_id: event_id,
                ts: obj
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                kind: "assistant_message".to_string(),
                agent: "claude_code".to_string(),
                provider: "anthropic".to_string(),
                model,
                session_id: session_id.clone().unwrap(),
                turn_index: None,
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
                duration_ms: None,
                tool_calls: aggregate,
                files_touched: Vec::new(),
                content_excerpt: excerpt,
                references: refs,
                source_file: Some(ctx.source_file.clone()),
                source_byte_offset: Some(offset),
                pricing_mode: Some("subscription".to_string()),
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
            if uuid.is_none() || session_id.is_none() {
                skipped += 1;
                continue;
            }
            let uuid = uuid.unwrap().to_string();
            let event_id = match dedupe_id_for(&session_id, &uuid, offset, &mut ancestors) {
                Some(id) => id,
                None => {
                    skipped += 1;
                    continue;
                }
            };
            let excerpt = extract_excerpt(&content);
            let refs = detect_event_references(&collect_ref_text(&content));
            sink.push(RawEvent {
                source_event_id: event_id,
                ts: obj
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                kind: "user_message".to_string(),
                agent: "claude_code".to_string(),
                provider: "anthropic".to_string(),
                model: last_model.clone(),
                session_id: session_id.clone().unwrap(),
                turn_index: None,
                parent_event_id: obj
                    .get("parentUuid")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                cwd: cwd.clone(),
                git: None,
                tokens: None,
                duration_ms: None,
                tool_calls: std::collections::BTreeMap::new(),
                files_touched: Vec::new(),
                content_excerpt: excerpt,
                references: refs,
                source_file: Some(ctx.source_file.clone()),
                source_byte_offset: Some(offset),
                pricing_mode: Some("subscription".to_string()),
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
            skipped += 1;
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
        agent: "claude_code".to_string(),
        server,
        name,
        turn_index: None,
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
