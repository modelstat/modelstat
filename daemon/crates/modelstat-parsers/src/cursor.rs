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

use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use modelstat_redact::redact;
use modelstat_wire::{source_event_id, EventSource, RawEvent};
use rusqlite::{Connection, OpenFlags};

use crate::skips::{unknown_record_event, SkipLedger, UnknownRecord};
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
    //
    // The `-wal` and `-shm` sidecars come TOO. Cursor runs the store in WAL
    // mode, so the main file alone is not a database: with a dirty write-ahead
    // log, opening a copy of it fails outright ("unable to open database file",
    // SQLITE_CANTOPEN) — and on the one occasion it does open, because the WAL
    // happened to be checkpointed, it is missing every conversation still
    // living in the log. Measured on a real install: main-file-only was
    // unreadable, while the three files together read 408 records.
    let result = with_snapshot(&ctx.source_file, read_bubbles)?;
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
    let mut skips = SkipLedger::default();
    let mut current_composer = String::new();
    let mut turn_index: u64 = 0;
    let mut saw_user_prompt = false;
    // This bubble's position in ITS conversation's record list — the `seq` a
    // transcript parser reads off a line number. The store is key/value, so the
    // order is the one established by the sort above rather than one written
    // down; it is a total order and the same one on every read, which is what
    // the field claims and all it claims.
    let mut seq: u64 = 0;

    for b in bubbles {
        // Counted BEFORE every filter below, and reset on the conversation
        // boundary the sort guarantees is contiguous: an ordinal names where a
        // record sat among its neighbours, so one this parser has no arm for
        // must still cost its neighbours their positions. Resetting here rather
        // than after the filters also stops a leftover `turn_index` from the
        // PREVIOUS conversation reaching the unmodelled-record arm below.
        if b.composer_id != current_composer {
            current_composer.clone_from(&b.composer_id);
            turn_index = 0;
            saw_user_prompt = false;
            seq = 0;
        }
        seq += 1;
        let kind = match b.kind {
            BUBBLE_TYPE_USER => "user_message",
            BUBBLE_TYPE_ASSISTANT => "assistant_message",
            // A bubble type this parser has no arm for. Cursor's discriminator
            // is a bare integer, so the ledger key is that integer as written —
            // there is no name to report, and inventing one would be a guess
            // about a record we by definition do not understand. This exact
            // branch is where the `ai_code_hashes` move went to die.
            other => {
                let observed = other.to_string();
                skips.drop_record(&ctx.source_file, &observed);
                if !b.composer_id.is_empty() && !b.created_at.is_empty() {
                    events.push(unknown_record_event(UnknownRecord {
                        kind: &observed,
                        source_event_id: source_event_id(
                            &ctx.device_id,
                            &EventSource::LineUuid {
                                line_uuid: &b.bubble_id,
                            },
                        ),
                        agent: "cursor",
                        provider: "cursor",
                        session_id: b.composer_id.clone(),
                        ts: b.created_at.clone(),
                        turn_index: Some(turn_index),
                        // A bubble is a message record; none of the fields this
                        // parser reads states an elapsed time.
                        duration_ms: None,
                        source_file: &ctx.source_file,
                        source_byte_offset: None,
                        seq: Some(seq),
                    }));
                }
                continue;
            }
        };
        let text = b.text.trim();
        if text.is_empty() || b.composer_id.is_empty() || b.created_at.is_empty() {
            continue;
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
            seq: Some(seq),
            started_at: None,
            first_token_at: None,
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
            actor_id: None,
            recipient_actor_id: None,
            turn_index: Some(turn_index),
            parent_event_id: None,
            cwd: None,
            git: None,
            // Every observed bubble reports `{input:0, output:0}` — state no
            // usage rather than record zeros as fact.
            tokens: None,
            tokens_unmapped: BTreeMap::new(),
            duration_ms: None,
            tool_calls: std::collections::BTreeMap::new(),
            files_touched: Vec::new(),
            tool_paths: Vec::new(),
            content_excerpt: Some(cleaned),
            content_bytes: Some(content_bytes),
            reasoning_excerpt: None,
            reasoning_bytes: None,
            references: None,
            source_file: Some(ctx.source_file.clone()),
            source_byte_offset: None,
            redactions: Default::default(),
            // Cursor bills its own flat plan; `provider` here is `cursor`,
            // not a model vendor, so there is no metered path to confuse it
            // with. These rows also carry no tokens, so the mode moves no money
            // either way — it is stated for the contract, not for the maths.
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
        skipped_kinds: skips.into_counts(),
        session_actors: Default::default(),
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

/// Run `read` against a private byte-snapshot of a Cursor store.
///
/// The `-wal` and `-shm` sidecars come along. Cursor runs the store in WAL
/// mode, so the main file alone is not a database: with a dirty write-ahead log
/// a copy of it cannot be opened at all, and on the occasion it can — because
/// the log happened to be checkpointed — it is missing everything still living
/// in that log. The live file is never opened (plan D6), so Cursor's own lock
/// is never contended.
pub(crate) fn with_snapshot<T>(
    source: &str,
    read: impl FnOnce(&std::path::Path) -> rusqlite::Result<T>,
) -> std::io::Result<rusqlite::Result<T>> {
    let tmp = std::env::temp_dir().join(format!(
        "modelstat-cursor-{}-{}.vscdb",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, fs::read(source)?)?;
    // Best-effort: a checkpointed store has no sidecars and is self-contained.
    for suffix in ["-wal", "-shm"] {
        if let Ok(bytes) = fs::read(format!("{source}{suffix}")) {
            let _ = fs::write(format!("{}{suffix}", tmp.to_string_lossy()), bytes);
        }
    }
    let out = read(&tmp);
    // SQLite may have checkpointed the copy's log on open, so clear all three.
    let _ = fs::remove_file(&tmp);
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", tmp.to_string_lossy()));
    }
    Ok(out)
}

/// Read an ALLOWLISTED set of `ItemTable` keys from a Cursor store.
///
/// Allowlisted, never a prefix scan: `cursorAuth/accessToken` and
/// `cursorAuth/refreshToken` sit in this very table, and a probe that swept
/// `cursorAuth*` would pull live credentials into memory on its way past the
/// one field it wanted. Only exactly-named keys are read.
pub(crate) fn read_item_table(
    source: &str,
    keys: &[&str],
) -> std::collections::BTreeMap<String, String> {
    let wanted: Vec<String> = keys.iter().map(|k| (*k).to_string()).collect();
    with_snapshot(source, |db| {
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut out = std::collections::BTreeMap::new();
        for key in &wanted {
            let got: rusqlite::Result<String> = conn.query_row(
                "SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?1",
                [key],
                |r| r.get(0),
            );
            if let Ok(v) = got {
                let v = v.trim().trim_matches('"').to_string();
                if !v.is_empty() {
                    out.insert(key.clone(), v);
                }
            }
        }
        Ok(out)
    })
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default()
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

    /// Cursor runs its store in WAL mode. Copying only the main file — which is
    /// what this parser did until the sidecars came along — cannot even OPEN a
    /// store whose write-ahead log is dirty, so every Cursor session on the
    /// machine was silently unreadable.
    #[test]
    fn a_wal_mode_store_with_a_dirty_log_is_still_read() {
        let path = std::env::temp_dir().join(format!(
            "modelstat-cursor-wal-{}-{}.vscdb",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        // WAL, and never checkpoint: the rows stay in the sidecar.
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
            [],
        )
        .unwrap();
        for (bid, kind, text, ts) in [
            ("w1", 1, "in the log", "2026-06-20T10:00:00.000Z"),
            ("w2", 2, "also in the log", "2026-06-20T10:00:02.000Z"),
        ] {
            conn.execute(
                "INSERT INTO cursorDiskKV VALUES (?,?)",
                rusqlite::params![
                    format!("bubbleId:comp-wal:{bid}"),
                    format!(r#"{{"type":{kind},"text":"{text}","createdAt":"{ts}"}}"#)
                ],
            )
            .unwrap();
        }
        let wal = format!("{}-wal", path.to_string_lossy());
        assert!(
            std::fs::metadata(&wal)
                .map(|m| m.len() > 0)
                .unwrap_or(false),
            "the test's premise: rows are sitting in a dirty WAL"
        );

        // Parse with the writer still connected, exactly as a live Cursor is.
        let r = parse_cursor_tracking_db(&ParserContext::new("dev_1", path.to_string_lossy()))
            .expect("a WAL-mode store is readable");
        assert_eq!(r.events.len(), 2, "both logged rows are read");
        assert_eq!(r.events[0].content_excerpt.as_deref(), Some("in the log"));

        drop(conn);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.to_string_lossy()));
        }
    }

    /// The account probe reads this table, and live credentials are its
    /// neighbours. Only exactly-named keys may come back — never a prefix sweep.
    #[test]
    fn the_item_table_reader_takes_only_the_keys_it_was_given() {
        let path = std::env::temp_dir().join(format!(
            "modelstat-cursor-items-{}-{}.vscdb",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT UNIQUE, value BLOB)", [])
            .unwrap();
        for (k, v) in [
            ("cursorAuth/cachedEmail", "dev@example.test"),
            ("cursorAuth/cachedSignUpType", "Google"),
            // The neighbour that must never be read.
            (
                "cursorAuth/accessToken",
                "eyJhbGciOiJIUzI1NiJ9.secret.value",
            ),
            (
                "cursorAuth/refreshToken",
                "eyJhbGciOiJIUzI1NiJ9.other.value",
            ),
        ] {
            conn.execute(
                "INSERT INTO ItemTable VALUES (?1,?2)",
                rusqlite::params![k, v],
            )
            .unwrap();
        }
        drop(conn);

        let got = read_item_table(
            &path.to_string_lossy(),
            &["cursorAuth/cachedEmail", "cursorAuth/cachedSignUpType"],
        );
        assert_eq!(
            got.get("cursorAuth/cachedEmail").map(String::as_str),
            Some("dev@example.test")
        );
        assert_eq!(got.len(), 2, "exactly the two keys asked for");
        let dumped = format!("{got:?}");
        assert!(
            !dumped.contains("eyJ") && !dumped.contains("Token"),
            "a token sitting in the same table never comes back: {dumped}"
        );

        // A key that does not exist is simply absent, not an error.
        assert!(read_item_table(&path.to_string_lossy(), &["nope"]).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn without_a_floor_every_message_ships() {
        let path = db_with(B);
        let r = parse_cursor_tracking_db(&ParserContext::new("dev_1", path.clone())).unwrap();
        assert_eq!(r.events.len(), 4);
        std::fs::remove_file(path).ok();
    }

    /// A key/value store has no line numbers, so the order is the one the sort
    /// above establishes — and `seq` counts positions in THAT, per conversation,
    /// before any floor applies. So the ordinal a message carries on a resumed
    /// scan is the one it carried on the first, exactly as `turn_index` is.
    #[test]
    fn seq_counts_positions_in_the_conversation_and_a_floor_does_not_renumber() {
        let path = db_with(B);
        let full = parse_cursor_tracking_db(&ParserContext::new("dev_1", path.clone())).unwrap();
        assert_eq!(
            full.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(3), Some(4)]
        );

        let floor = chrono::DateTime::parse_from_rfc3339("2026-06-20T10:30:00.000Z")
            .unwrap()
            .timestamp_millis();
        let resumed = parse_cursor_tracking_db(
            &ParserContext::new("dev_1", path.clone()).with_since_ms(Some(floor)),
        )
        .unwrap();
        assert_eq!(
            resumed.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![Some(3), Some(4)],
            "not renumbered to 1 by the floor"
        );
        std::fs::remove_file(path).ok();
    }

    /// Each conversation is numbered from its own start: the ordinal answers
    /// "where in THIS conversation", and a store-wide counter would make it
    /// depend on how many other chats happen to sort first.
    #[test]
    fn each_conversation_numbers_from_its_own_start() {
        let path = db_with(B);
        let conn = Connection::open(&path).unwrap();
        for (bid, kind, text, ts) in [
            ("z1", 1, "other chat", "2026-06-20T12:00:00.000Z"),
            ("z2", 2, "other reply", "2026-06-20T12:00:03.000Z"),
        ] {
            conn.execute(
                "INSERT INTO cursorDiskKV VALUES (?,?)",
                rusqlite::params![
                    format!("bubbleId:comp-2:{bid}"),
                    format!(r#"{{"type":{kind},"text":"{text}","createdAt":"{ts}"}}"#)
                ],
            )
            .unwrap();
        }
        drop(conn);

        let r = parse_cursor_tracking_db(&ParserContext::new("dev_1", path.clone())).unwrap();
        let by_session: Vec<(&str, Option<u64>)> = r
            .events
            .iter()
            .map(|e| (e.session_id.as_str(), e.seq))
            .collect();
        assert_eq!(
            by_session,
            vec![
                ("comp-1", Some(1)),
                ("comp-1", Some(2)),
                ("comp-1", Some(3)),
                ("comp-1", Some(4)),
                ("comp-2", Some(1)),
                ("comp-2", Some(2)),
            ]
        );
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
