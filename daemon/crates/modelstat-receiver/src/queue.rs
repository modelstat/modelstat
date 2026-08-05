//! `FileQueueStore` — a zero-native-dependency durable queue for the loopback
//! receiver, a port of `packages/daemon-core/src/node/file-queue-store.ts` +
//! the `QueueItem` / `QueueStore` contract (`queue/index.ts`).
//!
//! Persists queue state as ONE JSON document; every mutation writes atomically
//! (temp file → rename, keeping a single `.bak` generation) so a crash mid-write
//! can't corrupt the snapshot. The agent holds at most a few thousand unsynced
//! events (the upload loop drains every few seconds), so an in-memory map + full
//! rewrite is simpler + crash-safer than a native SQLite binding.
//!
//! On-disk format (v2): `{ "version": 2, "items": { <sourceEventId>: QueueItem } }`.
//! On load we drop `synced` items older than a day (past the idempotency window
//! their provenance is dead weight). v1 snapshots (pre-AGENTS `tool` key) are
//! dropped wholesale — re-scanning re-enqueues. Migration is intentionally lossy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use modelstat_parsers::ToolCallDraft;
use modelstat_wire::RawEvent;
use serde::{Deserialize, Serialize};

/// Drop `synced` items whose last event is older than this on load.
const SENT_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// One queued event awaiting upload. Keyed on `source_event_id` (the dedupe key
/// that must match the upcoming event's id). Port of TS `QueueItem`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub source_event_id: String,
    pub session_id: String,
    pub agent: String,
    pub event: RawEvent,
    pub last_event_ts_ms: i64,
    pub synced: bool,
    /// The batch that carried this item, or `null` while unsent.
    pub sent_batch_id: Option<String>,
    /// Per-call tool invocations born from `event` (same privacy contract as
    /// `ToolCallWire`). Absent for runtimes without tool-call data + old rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDraft>>,
}

/// The durable queue contract (put / has / list-unsent / mark-sent / count).
/// Async to match the TS `QueueStore` interface (the receiver awaits it); the
/// file impl below runs sync under the hood (the files are tiny).
pub trait QueueStore {
    async fn put(&self, item: QueueItem) -> std::io::Result<()>;
    async fn has(&self, source_event_id: &str) -> bool;
    async fn list_unsent_by_session(&self) -> BTreeMap<String, Vec<QueueItem>>;
    async fn mark_sent(
        &self,
        source_event_ids: &[String],
        batch_id: Option<&str>,
    ) -> std::io::Result<()>;
    async fn count_unsent(&self) -> usize;
}

/// Tolerant snapshot read target — `items` stays UNTYPED here so that a valid-JSON
/// but wrong-version (v1) file parses cleanly and is then dropped wholesale (like
/// the TS `JSON.parse` → `version === 2` check), rather than a strict typed map
/// treating a v1 row as a corrupt-JSON error and diverting to the `.bak`.
#[derive(Deserialize)]
struct RawSnapshot {
    version: Option<u32>,
    #[serde(default)]
    items: Option<serde_json::Map<String, serde_json::Value>>,
}

impl RawSnapshot {
    /// The v2 items (or `None` for a non-v2 / item-less snapshot). `drop_ttl`
    /// applies the synced-TTL filter (the main-file load path; the `.bak` path
    /// loads everything, matching the TS). A row that no longer deserializes as a
    /// `QueueItem` (schema drift) is skipped rather than sinking the whole load.
    fn into_items(self, now_ms: i64, drop_ttl: bool) -> Option<BTreeMap<String, QueueItem>> {
        if self.version != Some(2) {
            return None;
        }
        let raw_items = self.items?;
        let mut out = BTreeMap::new();
        for (k, v) in raw_items {
            if let Ok(item) = serde_json::from_value::<QueueItem>(v) {
                if drop_ttl && item.synced && now_ms - item.last_event_ts_ms > SENT_TTL_MS {
                    continue;
                }
                out.insert(k, item);
            }
        }
        Some(out)
    }
}

/// What gets serialized back out (always version 2).
#[derive(Serialize)]
struct Snapshot<'a> {
    version: u32,
    items: &'a BTreeMap<String, QueueItem>,
}

struct Inner {
    items: BTreeMap<String, QueueItem>,
    loaded: bool,
}

/// The single-JSON-document durable queue. Clone-share it behind an `Arc` across
/// the receiver's request handlers + drain loop; a `Mutex` serializes mutations
/// so overlapping writes can't race on the temp-file / rename swap.
pub struct FileQueueStore {
    file_path: PathBuf,
    inner: Mutex<Inner>,
}

/// `<path><suffix>` (e.g. `queue.json` + `.bak` → `queue.json.bak`), matching the
/// TS `${filePath}.bak` (append, NOT replace-extension).
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl FileQueueStore {
    pub fn new(file_path: impl Into<PathBuf>) -> Self {
        FileQueueStore {
            file_path: file_path.into(),
            inner: Mutex::new(Inner {
                items: BTreeMap::new(),
                loaded: false,
            }),
        }
    }

    /// Lazily load the snapshot on first access (construction stays cheap). A
    /// missing file is first boot; a corrupt/unreadable file falls back to the
    /// `.bak`; any other outcome leaves the map empty for the next mutation to
    /// overwrite atomically. Never errors (always reaches a loaded state).
    fn ensure_loaded(&self, inner: &mut Inner) {
        if inner.loaded {
            return;
        }
        inner.loaded = true;
        let now = now_ms();
        match std::fs::read_to_string(&self.file_path) {
            Ok(raw) => match serde_json::from_str::<RawSnapshot>(&raw) {
                // Valid JSON: load iff v2 (with the TTL drop); v1/other → empty,
                // and NO .bak fallback (the file is intact, just stale-format).
                Ok(snap) => {
                    if let Some(items) = snap.into_items(now, true) {
                        inner.items = items;
                    }
                }
                // Corrupt JSON → try the .bak once (loads everything, no TTL).
                Err(_) => self.load_from_bak(inner, now),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => { /* first boot */ }
            Err(_) => self.load_from_bak(inner, now),
        }
    }

    fn load_from_bak(&self, inner: &mut Inner, now_ms: i64) {
        let bak = sibling(&self.file_path, ".bak");
        if let Ok(raw) = std::fs::read_to_string(&bak) {
            if let Ok(snap) = serde_json::from_str::<RawSnapshot>(&raw) {
                if let Some(items) = snap.into_items(now_ms, false) {
                    inner.items = items;
                }
            }
        }
    }

    /// Atomically rewrite the snapshot: write `.tmp`, rotate the current file to
    /// `.bak`, promote `.tmp`. A missing prior file (first write) is fine.
    fn persist(&self, inner: &Inner) -> std::io::Result<()> {
        let snap = Snapshot {
            version: 2,
            items: &inner.items,
        };
        let body = serde_json::to_string(&snap)?;
        if let Some(dir) = self.file_path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let tmp = sibling(&self.file_path, ".tmp");
        std::fs::write(&tmp, body)?;
        match std::fs::rename(&self.file_path, sibling(&self.file_path, ".bak")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::rename(&tmp, &self.file_path)
    }
}

impl QueueStore for FileQueueStore {
    async fn put(&self, item: QueueItem) -> std::io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        self.ensure_loaded(&mut inner);
        if inner.items.contains_key(&item.source_event_id) {
            return Ok(()); // idempotent — dedupe by source_event_id
        }
        inner.items.insert(item.source_event_id.clone(), item);
        self.persist(&inner)
    }

    async fn has(&self, source_event_id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        self.ensure_loaded(&mut inner);
        inner.items.contains_key(source_event_id)
    }

    async fn list_unsent_by_session(&self) -> BTreeMap<String, Vec<QueueItem>> {
        let mut inner = self.inner.lock().unwrap();
        self.ensure_loaded(&mut inner);
        let mut unsent: Vec<QueueItem> = inner
            .items
            .values()
            .filter(|i| !i.synced)
            .cloned()
            .collect();
        // (session_id asc, then last_event_ts_ms asc) so each session's turns are
        // in chronological order — matches the TS sort.
        unsent.sort_by(|a, b| {
            a.session_id
                .cmp(&b.session_id)
                .then(a.last_event_ts_ms.cmp(&b.last_event_ts_ms))
        });
        let mut out: BTreeMap<String, Vec<QueueItem>> = BTreeMap::new();
        for it in unsent {
            out.entry(it.session_id.clone()).or_default().push(it);
        }
        out
    }

    async fn mark_sent(
        &self,
        source_event_ids: &[String],
        batch_id: Option<&str>,
    ) -> std::io::Result<()> {
        if source_event_ids.is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.lock().unwrap();
        self.ensure_loaded(&mut inner);
        for id in source_event_ids {
            if let Some(it) = inner.items.get_mut(id) {
                it.synced = true;
                it.sent_batch_id = batch_id.map(str::to_string);
            }
        }
        self.persist(&inner)
    }

    async fn count_unsent(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        self.ensure_loaded(&mut inner);
        inner.items.values().filter(|i| !i.synced).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "modelstat-queue-{}-{tag}/queue.json",
            std::process::id()
        ))
    }

    fn raw_event(id: &str, session: &str) -> RawEvent {
        RawEvent {
            content_bytes: None,
            source_event_id: id.into(),
            ts: "2026-07-16T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: session.into(),
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: None,
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            pricing_mode: "subscription".to_string(),
        }
    }

    fn item(id: &str, session: &str, ts_ms: i64) -> QueueItem {
        QueueItem {
            source_event_id: id.into(),
            session_id: session.into(),
            agent: "claude_code".into(),
            event: raw_event(id, session),
            last_event_ts_ms: ts_ms,
            synced: false,
            sent_batch_id: None,
            tool_calls: None,
        }
    }

    #[tokio::test]
    async fn put_is_idempotent_and_survives_reload() {
        let path = tmp_path("reload");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let q = FileQueueStore::new(&path);
        q.put(item("e1", "s1", 100)).await.unwrap();
        q.put(item("e1", "s1", 100)).await.unwrap(); // dupe — ignored
        assert!(q.has("e1").await);
        assert_eq!(q.count_unsent().await, 1);

        // A fresh store on the same path reloads the persisted snapshot.
        let q2 = FileQueueStore::new(&path);
        assert!(q2.has("e1").await);
        assert_eq!(q2.count_unsent().await, 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn list_unsent_groups_and_sorts_and_mark_sent_excludes() {
        let path = tmp_path("list");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let q = FileQueueStore::new(&path);
        q.put(item("b", "s2", 5)).await.unwrap();
        q.put(item("a", "s1", 20)).await.unwrap();
        q.put(item("c", "s1", 10)).await.unwrap();

        let grouped = q.list_unsent_by_session().await;
        assert_eq!(
            grouped.keys().cloned().collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );
        // s1's items in chronological order (ts 10 before 20).
        let s1: Vec<&str> = grouped["s1"]
            .iter()
            .map(|i| i.source_event_id.as_str())
            .collect();
        assert_eq!(s1, vec!["c", "a"]);

        // Marking s1's items sent removes them from the unsent view.
        q.mark_sent(&["a".into(), "c".into()], Some("batch1"))
            .await
            .unwrap();
        assert_eq!(q.count_unsent().await, 1);
        let grouped = q.list_unsent_by_session().await;
        assert_eq!(grouped.keys().cloned().collect::<Vec<_>>(), vec!["s2"]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ttl_drops_old_synced_items_on_load_but_keeps_fresh_and_unsent() {
        let now = 10_000_000_000i64;
        let mut items = serde_json::Map::new();
        let mut add = |it: QueueItem| {
            items.insert(
                it.source_event_id.clone(),
                serde_json::to_value(&it).unwrap(),
            );
        };
        // synced + older than a day → dropped.
        let mut old = item("old", "s", now - SENT_TTL_MS - 1);
        old.synced = true;
        add(old);
        // synced but recent → kept.
        let mut fresh = item("fresh", "s", now - 1000);
        fresh.synced = true;
        add(fresh);
        // unsent + ancient → kept (TTL only applies to synced items).
        add(item("unsent", "s", 0));

        let snap = RawSnapshot {
            version: Some(2),
            items: Some(items),
        };
        let loaded = snap.into_items(now, true).unwrap();
        assert!(!loaded.contains_key("old"));
        assert!(loaded.contains_key("fresh"));
        assert!(loaded.contains_key("unsent"));
    }

    #[test]
    fn a_v1_snapshot_is_dropped_wholesale() {
        let raw = r#"{"version":1,"items":{"e1":{"source_event_id":"e1"}}}"#;
        let snap: RawSnapshot = serde_json::from_str(raw).unwrap();
        assert!(snap.into_items(0, true).is_none()); // non-v2 → empty
    }

    #[tokio::test]
    async fn recovers_from_a_valid_bak_when_the_main_file_is_corrupt() {
        let path = tmp_path("bak");
        let dir = path.parent().unwrap();
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        // Corrupt main + a valid .bak carrying one item.
        std::fs::write(&path, b"{ this is not json").unwrap();
        let mut items = BTreeMap::new();
        items.insert("e9".to_string(), item("e9", "s1", 1));
        let bak_body = serde_json::to_string(&Snapshot {
            version: 2,
            items: &items,
        })
        .unwrap();
        std::fs::write(sibling(&path, ".bak"), bak_body).unwrap();

        let q = FileQueueStore::new(&path);
        assert!(q.has("e9").await); // loaded from .bak
        let _ = std::fs::remove_dir_all(dir);
    }
}
