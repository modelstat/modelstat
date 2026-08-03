//! Cursor tracking-DB parser — a port of `packages/parsers/src/cursor/index.ts`.
//!
//! One `assistant_message` per AI-attributed `ai_code_hashes` row; tokens null;
//! session = `conversationId`; id keyed `<file>#<hash>` + timestamp. As-built this
//! parser is DORMANT (feature §7.1) — the scan loop only enumerates it behind
//! `MODELSTAT_ENABLE_CURSOR_PARSER=1` — but its output shape is frozen here.
//!
//! Per plan D6 we open a byte-snapshot COPY read-only (read file → temp → open),
//! never the live file, so we never lock a DB Cursor has open.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, SecondsFormat, Utc};
use modelstat_wire::{source_event_id, EventSource, RawEvent};
use rusqlite::{Connection, OpenFlags};

use crate::auth_mode;
use crate::types::{ParseResult, ParseStats, ParserContext};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Row {
    hash: String,
    model: Option<String>,
    conversation_id: String,
    timestamp: i64,
}

/// Read the Cursor tracking DB and emit one RawEvent per AI-attributed code hash.
pub fn parse_cursor_tracking_db(ctx: &ParserContext) -> std::io::Result<ParseResult> {
    // Snapshot the DB to a temp copy (plan D6) so we never lock the live file.
    let bytes = fs::read(&ctx.source_file)?;
    let tmp = std::env::temp_dir().join(format!(
        "modelstat-cursor-{}-{}.vscdb",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, &bytes)?;

    let result = read_rows(&tmp);
    let _ = fs::remove_file(&tmp);
    let rows = result.map_err(|e| std::io::Error::other(e.to_string()))?;

    let mut events: Vec<RawEvent> = Vec::new();
    let mut raw_lines: u64 = 0;
    for r in rows {
        raw_lines += 1;
        // Mirror the JS truthy guard (`!conversationId || !timestamp`): SQL already
        // filtered NULLs, so this drops only "" / 0.
        if r.conversation_id.is_empty() || r.timestamp == 0 {
            continue;
        }
        let ts = DateTime::<Utc>::from_timestamp_millis(r.timestamp)
            .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_default();
        events.push(RawEvent {
            source_event_id: source_event_id(
                &ctx.device_id,
                &EventSource::File {
                    file: &format!("{}#{}", ctx.source_file, r.hash),
                    byte_offset: r.timestamp as u64,
                },
            ),
            ts,
            kind: "assistant_message".to_string(),
            agent: "cursor".to_string(),
            provider: "cursor".to_string(),
            model: r.model,
            session_id: r.conversation_id,
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: None,
            duration_ms: None,
            tool_calls: std::collections::BTreeMap::new(),
            files_touched: Vec::new(),
            content_excerpt: None,
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
        tool_calls: Vec::new(), // ai_code_hashes has no tool-call data — always empty
        script_contexts: Vec::new(),
        stats: ParseStats {
            raw_lines,
            emitted_events: emitted,
            skipped: raw_lines - emitted,
        },
        source_file: ctx.source_file.clone(),
    })
}

fn read_rows(path: &std::path::Path) -> rusqlite::Result<Vec<Row>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT hash, source, model, requestId, conversationId, timestamp
           FROM ai_code_hashes
          WHERE conversationId IS NOT NULL AND timestamp IS NOT NULL
          ORDER BY timestamp ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Row {
            hash: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            model: row.get::<_, Option<String>>(2)?,
            conversation_id: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            timestamp: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
        })
    })?;
    rows.collect()
}
