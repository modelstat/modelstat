//! Cursor chat parser — reads the conversation records Cursor keeps in its
//! global `state.vscdb`.
//!
//! REPLACES the old `ai_code_hashes` reader. That table produced text-less,
//! token-less assistant rows AND no longer exists: a current Cursor install has
//! it in neither `globalStorage/state.vscdb` nor any of its
//! `workspaceStorage/*/state.vscdb` (verified against a real install, 8 DBs,
//! zero hits), so the old path could only ever emit nothing.
//!
//! What Cursor actually stores: `cursorDiskKV` rows keyed
//! `bubbleId:<composerId>:<bubbleId>`, each a JSON message ("bubble") with
//! `type` (1 = user, 2 = assistant), `text`, and an ISO `createdAt`. The
//! session is the composerId **read off the key** — `composerData:` header
//! lists exist but can be empty on a live conversation, and a conversation must
//! not disappear because its index row lagged.
//!
//! SPEC 0005: `text` ships VERBATIM — redaction is the only thing between the
//! DB and the wire, and nothing is ever cut short. Bubbles carry a
//! `tokenCount`, but it is `{input:0, output:0}` on every real row observed, so
//! this parser states no tokens rather than inventing zeros.
//!
//! `discover_jobs` walks the store on every scan (macOS / Linux / Windows user
//! data paths). It was unwalked for this parser's whole life — the docs claimed
//! `MODELSTAT_ENABLE_CURSOR_PARSER=1` gated it, and no such flag ever existed in
//! the code, so Cursor was simply never read.
//!
//! Per plan D6 we open a byte-snapshot COPY read-only (read file → temp → open),
//! never the live file, so we never lock a DB Cursor has open.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use modelstat_redact::redact;
use modelstat_wire::{source_event_id, EventSource, RawEvent};
use rusqlite::{Connection, OpenFlags};

use crate::auth_mode;
use crate::types::{ParseResult, ParseStats, ParserContext};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Cursor's own message-kind discriminator on a bubble record.
const BUBBLE_TYPE_USER: i64 = 1;
const BUBBLE_TYPE_ASSISTANT: i64 = 2;

struct Bubble {
    composer_id: String,
    bubble_id: String,
    /// Cursor's `type`: 1 = user, 2 = assistant. Anything else is skipped.
    kind: i64,
    text: String,
    created_at: String,
}

/// Read the Cursor chat store and emit one RawEvent per message that carries
/// text. Bubbles with no text (tool/thinking-only assistant steps) are skipped:
/// they are not messages and would be empty rows.
pub fn parse_cursor_tracking_db(ctx: &ParserContext) -> std::io::Result<ParseResult> {
    // Snapshot the DB to a temp copy (plan D6) so we never lock the live file.
    let bytes = fs::read(&ctx.source_file)?;
    let tmp = std::env::temp_dir().join(format!(
        "modelstat-cursor-{}-{}.vscdb",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, &bytes)?;

    let result = read_bubbles(&tmp);
    let _ = fs::remove_file(&tmp);
    let mut bubbles = result.map_err(|e| std::io::Error::other(e.to_string()))?;

    let raw_lines = bubbles.len() as u64;

    // Conversation order: by timestamp within a composer, bubble id breaking
    // same-millisecond ties so a re-scan always derives identical turn ordinals.
    bubbles.sort_by(|a, b| {
        a.composer_id
            .cmp(&b.composer_id)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.bubble_id.cmp(&b.bubble_id))
    });

    let mut events: Vec<RawEvent> = Vec::new();
    let mut current_composer = String::new();
    let mut turn_index: u64 = 0;
    let mut saw_user_prompt = false;

    for b in bubbles {
        let kind = match b.kind {
            BUBBLE_TYPE_USER => "user_message",
            BUBBLE_TYPE_ASSISTANT => "assistant_message",
            _ => continue,
        };
        let text = b.text.trim();
        if text.is_empty() || b.composer_id.is_empty() || b.created_at.is_empty() {
            continue;
        }
        // A new conversation restarts the turn ordinal.
        if b.composer_id != current_composer {
            current_composer.clone_from(&b.composer_id);
            turn_index = 0;
            saw_user_prompt = false;
        }
        // A turn starts at each user message (SPEC 0005, as in the other
        // parsers); the assistant's replies inherit the ordinal.
        if kind == "user_message" {
            if saw_user_prompt {
                turn_index += 1;
            }
            saw_user_prompt = true;
        }
        // Already shipped. Applied HERE, after the ordinal above: the floor
        // must not renumber turns, or the same message would carry a different
        // `turn_index` on a resumed scan than it did on the first. A key/value
        // row carries no other cross-record state, so skipping the SEND this
        // late is free (positional parsers floor the send instead — see
        // `ParserContext::since_ms`).
        if let Some(floor) = ctx.since_ms {
            if bubble_ms(&b.created_at).is_some_and(|ms| ms < floor) {
                continue;
            }
        }
        // VERBATIM: redaction is the only transformation.
        let content_bytes = text.chars().count() as u64;
        let cleaned = redact(text, None).text;
        if cleaned.is_empty() {
            continue;
        }
        events.push(RawEvent {
            // Keyed by the bubble's own uuid: position-independent, so a
            // re-scan of a DB whose rows moved re-derives the same id and the
            // server upserts instead of duplicating.
            source_event_id: source_event_id(
                &ctx.device_id,
                &EventSource::LineUuid {
                    line_uuid: &b.bubble_id,
                },
            ),
            ts: b.created_at,
            kind: kind.to_string(),
            agent: "cursor".to_string(),
            provider: "cursor".to_string(),
            // Bubbles name no model; the account's plan covers the call.
            model: None,
            session_id: b.composer_id,
            turn_index: Some(turn_index),
            parent_event_id: None,
            cwd: None,
            git: None,
            // Every observed bubble reports `{input:0, output:0}` — state no
            // usage rather than record zeros as fact.
            tokens: None,
            duration_ms: None,
            tool_calls: std::collections::BTreeMap::new(),
            files_touched: Vec::new(),
            content_excerpt: Some(cleaned),
            content_bytes: Some(content_bytes),
            references: None,
            source_file: Some(ctx.source_file.clone()),
            source_byte_offset: None,
            // Cursor bills its own flat plan; `provider` here is `cursor`,
            // not a model vendor, so there is no metered path to confuse it
            // with. These rows also carry no tokens, so the mode moves no money
            // either way — it is stated for the contract, not for the maths.
            pricing_mode: auth_mode::PRICING_MODE_SUBSCRIPTION.to_string(),
        });
    }

    let emitted = events.len() as u64;
    Ok(ParseResult {
        events,
        // Cursor's bubble store carries no per-call tool telemetry in a shape
        // this parser can key — always empty.
        tool_calls: Vec::new(),
        script_contexts: Vec::new(),
        stats: ParseStats {
            raw_lines,
            emitted_events: emitted,
            skipped: raw_lines.saturating_sub(emitted),
        },
        source_file: ctx.source_file.clone(),
    })
}

/// A bubble's ISO `createdAt` as epoch millis — the coordinate the scan's
/// since-floor is expressed in. `None` when unparseable, which ships the row
/// (re-sending is an upsert; dropping would be data loss).
fn bubble_ms(created_at: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// `bubbleId:<composerId>:<bubbleId>` → the two ids. `None` for any other key
/// shape (the same store holds unrelated caches).
fn split_bubble_key(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("bubbleId:")?;
    let (composer, bubble) = rest.split_once(':')?;
    if composer.is_empty() || bubble.is_empty() {
        return None;
    }
    Some((composer.to_string(), bubble.to_string()))
}

fn read_bubbles(path: &std::path::Path) -> rusqlite::Result<Vec<Bubble>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt =
        conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'bubbleId:%'")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (key, value) = r?;
        let Some((composer_id, bubble_id)) = split_bubble_key(&key) else {
            continue;
        };
        // A record we cannot parse is skipped, never fatal: this store mixes
        // schema generations and unrelated caches under one table.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&value) else {
            continue;
        };
        out.push(Bubble {
            composer_id,
            bubble_id,
            kind: v
                .get("type")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0),
            text: v
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            created_at: v
                .get("createdAt")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bubble_keys_split_and_junk_is_ignored() {
        assert_eq!(
            split_bubble_key("bubbleId:comp-1:bub-1"),
            Some(("comp-1".into(), "bub-1".into()))
        );
        assert_eq!(split_bubble_key("composerData:comp-1"), None);
        assert_eq!(split_bubble_key("bubbleId:comp-only"), None);
        assert_eq!(split_bubble_key("bubbleId::bub-1"), None);
    }
}

#[cfg(test)]
mod since_floor_tests {
    use super::*;
    use rusqlite::Connection;

    fn db_with(bubbles: &[(&str, i64, &str, &str)]) -> String {
        let p = std::env::temp_dir().join(format!(
            "modelstat-cursor-floor-{}-{}.vscdb",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let c = Connection::open(&p).unwrap();
        c.execute(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
            [],
        )
        .unwrap();
        for (bid, kind, text, ts) in bubbles {
            c.execute(
                "INSERT INTO cursorDiskKV VALUES (?,?)",
                rusqlite::params![
                    format!("bubbleId:comp-1:{bid}"),
                    format!(r#"{{"type":{kind},"text":"{text}","createdAt":"{ts}"}}"#)
                ],
            )
            .unwrap();
        }
        drop(c);
        p.to_string_lossy().into_owned()
    }

    const B: &[(&str, i64, &str, &str)] = &[
        ("b1", 1, "first ask", "2026-06-20T10:00:00.000Z"),
        ("b2", 2, "first reply", "2026-06-20T10:00:05.000Z"),
        ("b3", 1, "second ask", "2026-06-20T11:00:00.000Z"),
        ("b4", 2, "second reply", "2026-06-20T11:00:04.000Z"),
    ];

    #[test]
    fn without_a_floor_every_message_ships() {
        let path = db_with(B);
        let r = parse_cursor_tracking_db(&ParserContext::new("dev_1", path.clone())).unwrap();
        assert_eq!(r.events.len(), 4);
        std::fs::remove_file(path).ok();
    }

    /// The floor is what keeps a constantly-rewritten KV store from re-shipping
    /// the user's whole chat history on every scan.
    #[test]
    fn a_floor_ships_only_what_came_after_it() {
        let path = db_with(B);
        let floor = chrono::DateTime::parse_from_rfc3339("2026-06-20T10:30:00.000Z")
            .unwrap()
            .timestamp_millis();
        let r = parse_cursor_tracking_db(
            &ParserContext::new("dev_1", path.clone()).with_since_ms(Some(floor)),
        )
        .unwrap();
        let texts: Vec<&str> = r
            .events
            .iter()
            .map(|e| e.content_excerpt.as_deref().unwrap())
            .collect();
        assert_eq!(texts, vec!["second ask", "second reply"]);
        // Turn ordinals still come from the WHOLE conversation, so a resumed
        // scan numbers turns exactly as a first scan did.
        assert_eq!(
            r.events[0].turn_index,
            Some(1),
            "not renumbered to 0 by the floor"
        );
        std::fs::remove_file(path).ok();
    }
}
