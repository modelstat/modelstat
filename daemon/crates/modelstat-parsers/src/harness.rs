//! Harness detection — which HOST application drove a session another
//! parser reads.
//!
//! BB (an agentic IDE / agent harness) runs coding agents as embedded
//! providers via their SDKs. The spawned agent writes its
//! own transcript in its own home — a BB-driven Claude Code session lands in
//! `~/.claude/projects/<dir>/<session>.jsonl` exactly like a plain CLI run —
//! so unlike Claude Desktop, the transcript's PATH says nothing about the
//! harness. What does is BB's own store: `~/.bb/bb.db` (SQLite, WAL mode)
//! records the provider's session id against each BB thread in
//! `events.provider_thread_id`. A session id named there was driven by BB,
//! and `agent` names the tool the human used (the `claude_desktop` precedent
//! in `discover_jobs`).
//!
//! The transcript itself only says `"entrypoint": "sdk-cli"` — "some SDK
//! embedder", never which one — so a session BB's store does not (or does not
//! yet) name stays honestly unlabelled as plain Claude Code.

use std::path::Path;

/// Provider session ids the BB install at `home` drove — every
/// `provider_thread_id` its store names, across providers.
///
/// Read from a WAL-safe byte-snapshot (BB runs the store in WAL mode; the
/// live file is never opened, so BB's own lock is never contended). Missing
/// store, unreadable store, or a schema this query no longer fits all yield
/// the empty set: no labels rather than wrong labels.
pub fn bb_session_ids_in(home: &Path) -> Vec<String> {
    let db = home.join(".bb/bb.db");
    if !db.is_file() {
        return Vec::new();
    }
    let read = crate::cursor::with_snapshot(&db.to_string_lossy(), |snap| {
        let conn = rusqlite::Connection::open_with_flags(
            snap,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT provider_thread_id FROM events \
             WHERE provider_thread_id IS NOT NULL AND provider_thread_id != ''",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<String>>>()
    });
    match read {
        Ok(Ok(ids)) => ids,
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture in BB's real shape: the `events` table as `bb.db` declares it
    /// (trimmed to the columns the reader touches plus its real key columns).
    fn write_bb_db(home: &Path, ids: &[Option<&str>]) {
        let dir = home.join(".bb");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("bb.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE events (
                id TEXT PRIMARY KEY NOT NULL,
                thread_id TEXT NOT NULL,
                provider_thread_id TEXT,
                sequence INTEGER NOT NULL,
                type TEXT NOT NULL,
                data TEXT DEFAULT '{}' NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        for (i, id) in ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO events (id, thread_id, provider_thread_id, sequence, type, data, created_at) \
                 VALUES (?1, 'thr_x', ?2, ?3, 'turn/started', '{}', 0)",
                rusqlite::params![format!("evt_{i}"), id, i as i64],
            )
            .unwrap();
        }
    }

    fn temp_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("modelstat-bb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn ids_are_read_distinct_and_null_free() {
        let home = temp_home("read");
        write_bb_db(
            &home,
            &[
                Some("49ce134e-2545-4ac0-a319-0557058ae4ef"),
                Some("49ce134e-2545-4ac0-a319-0557058ae4ef"),
                Some("f8aa719e-f642-4e8d-804c-685271593e62"),
                None,
                Some(""),
            ],
        );
        let mut ids = bb_session_ids_in(&home);
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "49ce134e-2545-4ac0-a319-0557058ae4ef",
                "f8aa719e-f642-4e8d-804c-685271593e62"
            ]
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// BB runs the store in WAL mode: rows still living in the log must be
    /// seen, exactly as the Cursor store's snapshot promises.
    #[test]
    fn a_wal_mode_store_with_a_dirty_log_is_still_read() {
        let home = temp_home("wal");
        write_bb_db(&home, &[]);
        let db = home.join(".bb/bb.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute(
            "INSERT INTO events (id, thread_id, provider_thread_id, sequence, type, data, created_at) \
             VALUES ('evt_w', 'thr_x', 'wal-only-session', 0, 'turn/started', '{}', 0)",
            [],
        )
        .unwrap();
        let wal = format!("{}-wal", db.to_string_lossy());
        assert!(
            std::fs::metadata(&wal)
                .map(|m| m.len() > 0)
                .unwrap_or(false),
            "precondition: the row lives in the log"
        );
        assert_eq!(bb_session_ids_in(&home), vec!["wal-only-session"]);
        drop(conn);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_missing_or_foreign_store_yields_nothing() {
        let home = temp_home("none");
        assert!(bb_session_ids_in(&home).is_empty(), "no store at all");

        // A store whose schema this reader no longer fits: empty, not an error.
        let dir = home.join(".bb");
        std::fs::create_dir_all(&dir).unwrap();
        rusqlite::Connection::open(dir.join("bb.db"))
            .unwrap()
            .execute_batch("CREATE TABLE something_else (x TEXT);")
            .unwrap();
        assert!(bb_session_ids_in(&home).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
}
