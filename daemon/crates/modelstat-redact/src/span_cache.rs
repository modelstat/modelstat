//! A content-addressed cache in front of the layer-2 span classifier.
//!
//! Agent transcripts repeat themselves: the same tool output appears in turn
//! after turn, and a `PROCESSING_VERSION` bump re-reads the entire corpus
//! through the same model that already classified it. Classification is pure —
//! same text, same weights, same operating point ⇒ same spans — so paying
//! hundreds of milliseconds of inference for a text we have already answered is
//! pure waste. This module remembers the answers.
//!
//! What is cached is the model's RAW output (the [`NerToken`] list), never any
//! transform of the text: the splice/snap/merge logic downstream keeps running
//! on every call, so a cached answer is byte-identical to a computed one by
//! construction, and changes to that logic need no cache invalidation.
//!
//! Keys are `sha256(fingerprint ‖ text)`, where the fingerprint names the exact
//! weights + operating point (see the caller). A model swap or a recall-bias
//! change produces disjoint keys, so stale answers cannot survive either.
//!
//! Failure posture: the cache can only ever make things faster. Any SQLite
//! error disables it for the rest of the process (loudly, once) and every call
//! falls through to the wrapped model. A `None` from the model (unavailable /
//! could not answer) is NEVER cached — "no answer" is a transient condition,
//! and the fail-closed hold machinery upstream owns it.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::ner::{NerModel, NerToken};

/// Default size cap for the cache file — generous enough to hold the unique
/// texts of a very large corpus (spans are tiny; most answers are the empty
/// list), small next to the ~1 GB of model weights beside it.
pub const DEFAULT_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// How many inserts happen between size checks. A check is two PRAGMAs; the
/// stride just keeps it off the per-call path.
const SIZE_CHECK_STRIDE: u64 = 2048;

/// The on-disk store: one SQLite file, WAL mode, capped by [`SpanStore::open`]'s
/// `max_bytes` via oldest-used eviction.
pub struct SpanStore {
    conn: Mutex<Connection>,
    /// Model identity + operating point, mixed into every key.
    fingerprint: String,
    max_bytes: u64,
    inserts: AtomicU64,
    /// Latched on the first SQLite error — from then on the store answers
    /// nothing and stores nothing, and the wrapped model does all the work.
    dead: AtomicBool,
}

impl SpanStore {
    /// Open (or create) the cache at `path`. `fingerprint` must name the exact
    /// model + operating point the answers come from — two different models
    /// sharing one file is fine, two different models sharing one KEY is not.
    pub fn open(path: &Path, fingerprint: &str, max_bytes: u64) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        // auto_vacuum must be set before the first table exists to take effect
        // on a fresh file; on an existing file it is a harmless no-op.
        conn.execute_batch(
            "PRAGMA auto_vacuum = INCREMENTAL;\n\
             PRAGMA journal_mode = WAL;\n\
             PRAGMA synchronous = NORMAL;\n\
             CREATE TABLE IF NOT EXISTS spans (\n\
               key     BLOB PRIMARY KEY,\n\
               tokens  TEXT NOT NULL,\n\
               used_ms INTEGER NOT NULL\n\
             ) WITHOUT ROWID;",
        )
        .map_err(|e| e.to_string())?;
        Ok(SpanStore {
            conn: Mutex::new(conn),
            fingerprint: fingerprint.to_string(),
            max_bytes,
            inserts: AtomicU64::new(0),
            dead: AtomicBool::new(false),
        })
    }

    fn key(&self, text: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        // Length prefix so (fingerprint, text) pairs can never collide by
        // shifting bytes across the boundary.
        h.update((self.fingerprint.len() as u64).to_le_bytes());
        h.update(self.fingerprint.as_bytes());
        h.update(text.as_bytes());
        h.finalize().into()
    }

    /// One failure kills the cache for the process — logged once, never fatal.
    fn die(&self, op: &str, e: impl std::fmt::Display) {
        if !self.dead.swap(true, Ordering::Relaxed) {
            modelstat_log::log_warn!(
                "span cache disabled for this run ({op}: {e}) — redaction continues \
                 uncached at full model cost"
            );
        }
    }

    fn get(&self, text: &str) -> Option<Vec<NerToken>> {
        if self.dead.load(Ordering::Relaxed) {
            return None;
        }
        let key = self.key(text);
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let row: Option<String> = match conn
            .query_row("SELECT tokens FROM spans WHERE key = ?1", [&key[..]], |r| {
                r.get(0)
            })
            .optional()
        {
            Ok(v) => v,
            Err(e) => {
                self.die("read", e);
                return None;
            }
        };
        let json = row?;
        // Refresh recency; eviction orders by this. Best-effort.
        let _ = conn.execute(
            "UPDATE spans SET used_ms = ?2 WHERE key = ?1",
            rusqlite::params![&key[..], now_ms()],
        );
        match decode_tokens(&json) {
            Some(tokens) => Some(tokens),
            None => {
                // An undecodable row is a corrupt entry, not a corrupt cache:
                // drop it and recompute.
                let _ = conn.execute("DELETE FROM spans WHERE key = ?1", [&key[..]]);
                None
            }
        }
    }

    fn put(&self, text: &str, tokens: &[NerToken]) {
        if self.dead.load(Ordering::Relaxed) {
            return;
        }
        let key = self.key(text);
        let json = encode_tokens(tokens);
        {
            let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = conn.execute(
                "INSERT OR REPLACE INTO spans (key, tokens, used_ms) VALUES (?1, ?2, ?3)",
                rusqlite::params![&key[..], json, now_ms()],
            ) {
                self.die("write", e);
                return;
            }
        }
        if self.inserts.fetch_add(1, Ordering::Relaxed) % SIZE_CHECK_STRIDE == 0 {
            self.evict_if_over();
        }
    }

    /// When the file outgrows the cap, drop the least-recently-used eighth and
    /// hand the pages back (`incremental_vacuum`). An eighth per pass keeps the
    /// steady state comfortably under the cap without evicting in a busy loop.
    fn evict_if_over(&self) {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let bytes = |name: &str| -> u64 {
            conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, u64>(0))
                .unwrap_or(0)
        };
        if bytes("page_count") * bytes("page_size") <= self.max_bytes {
            return;
        }
        let evicted = conn
            .execute(
                "DELETE FROM spans WHERE key IN \
                 (SELECT key FROM spans ORDER BY used_ms \
                  LIMIT (SELECT count(*) / 8 + 1 FROM spans))",
                [],
            )
            .unwrap_or(0);
        let _ = conn.execute_batch("PRAGMA incremental_vacuum;");
        modelstat_log::log_info!("span cache over its size cap — evicted {evicted} oldest entries");
    }

    /// Entry count — test + `status` surface, not a hot path.
    pub fn len(&self) -> u64 {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.query_row("SELECT count(*) FROM spans", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Compact array-of-arrays: `[[entity, word, start|null, end|null], …]`.
/// Hand-rolled (no serde derives on [`NerToken`]) so the wire shape of the
/// cache is explicit and stable.
fn encode_tokens(tokens: &[NerToken]) -> String {
    let rows: Vec<serde_json::Value> = tokens
        .iter()
        .map(|t| serde_json::json!([t.entity, t.word, t.start, t.end]))
        .collect();
    serde_json::Value::Array(rows).to_string()
}

fn decode_tokens(json: &str) -> Option<Vec<NerToken>> {
    let rows: Vec<(String, String, Option<usize>, Option<usize>)> =
        serde_json::from_str(json).ok()?;
    Some(
        rows.into_iter()
            .map(|(entity, word, start, end)| NerToken {
                entity,
                word,
                start,
                end,
            })
            .collect(),
    )
}

/// A [`NerModel`] that answers from the store when it can and from the wrapped
/// model when it must. With no store it is a transparent pass-through, so
/// callers hold ONE type either way.
pub struct CachedNer<N> {
    inner: N,
    store: Option<SpanStore>,
}

impl<N: NerModel> CachedNer<N> {
    pub fn new(inner: N, store: Option<SpanStore>) -> Self {
        CachedNer { inner, store }
    }

    /// The wrapped model, for callers that need the concrete type back.
    pub fn inner(&self) -> &N {
        &self.inner
    }
}

impl<N: NerModel> NerModel for CachedNer<N> {
    fn classify(&self, text: &str) -> Option<Vec<NerToken>> {
        let Some(store) = &self.store else {
            return self.inner.classify(text);
        };
        if let Some(hit) = store.get(text) {
            return Some(hit);
        }
        let answer = self.inner.classify(text)?;
        store.put(text, &answer);
        Some(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ner::{ner_redact, UnavailableNer};
    use std::sync::atomic::AtomicUsize;

    /// Counts calls; answers a fixed span for "Katherine", nothing otherwise.
    struct Counting {
        calls: AtomicUsize,
    }
    impl Counting {
        fn new() -> Self {
            Counting {
                calls: AtomicUsize::new(0),
            }
        }
    }
    impl NerModel for Counting {
        fn classify(&self, text: &str) -> Option<Vec<NerToken>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Some(match text.find("Katherine") {
                Some(at) => vec![NerToken {
                    entity: "S-private_person".into(),
                    word: "Katherine".into(),
                    start: Some(at),
                    end: Some(at + 9),
                }],
                None => Vec::new(),
            })
        }
    }

    fn store(dir: &Path, fp: &str) -> SpanStore {
        SpanStore::open(&dir.join("cache.sqlite3"), fp, DEFAULT_MAX_BYTES).unwrap()
    }

    #[test]
    fn a_repeat_is_answered_once_and_identically() {
        let tmp = tempfile::tempdir().unwrap();
        let cached = CachedNer::new(Counting::new(), Some(store(tmp.path(), "fp1")));
        let text = "ping Katherine about the deploy";
        let first = ner_redact(&cached, text);
        let second = ner_redact(&cached, text);
        assert_eq!(first, second, "a cache hit must be byte-identical");
        assert!(first.text.contains("[REDACTED:PRIVATE_PERSON]"));
        assert_eq!(cached.inner().calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn empty_answers_are_cached_too() {
        // Most turns contain no PII at all — the empty answer IS the hot case.
        let tmp = tempfile::tempdir().unwrap();
        let cached = CachedNer::new(Counting::new(), Some(store(tmp.path(), "fp1")));
        assert_eq!(cached.classify("nothing sensitive here"), Some(Vec::new()));
        assert_eq!(cached.classify("nothing sensitive here"), Some(Vec::new()));
        assert_eq!(cached.inner().calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn no_answer_is_never_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(tmp.path(), "fp1");
        let cached = CachedNer::new(UnavailableNer, Some(st));
        assert_eq!(cached.classify("Katherine"), None);
        assert_eq!(cached.classify("Katherine"), None);
        assert!(
            cached.store.as_ref().unwrap().is_empty(),
            "a model that could not answer must leave no trace — the next call \
             has to reach the model again"
        );
    }

    #[test]
    fn the_fingerprint_scopes_every_key() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "ping Katherine";
        {
            let cached = CachedNer::new(Counting::new(), Some(store(tmp.path(), "model-A")));
            cached.classify(text);
        }
        // Same file, different fingerprint: must MISS and recompute.
        let cached = CachedNer::new(Counting::new(), Some(store(tmp.path(), "model-B")));
        cached.classify(text);
        assert_eq!(
            cached.inner().calls.load(Ordering::SeqCst),
            1,
            "an answer from different weights must never be served"
        );
    }

    #[test]
    fn answers_survive_a_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let cached = CachedNer::new(Counting::new(), Some(store(tmp.path(), "fp")));
            cached.classify("ping Katherine");
        }
        // A new process (auto-update restart, mid-backfill) reopens the file.
        let cached = CachedNer::new(Counting::new(), Some(store(tmp.path(), "fp")));
        let out = cached.classify("ping Katherine").unwrap();
        assert_eq!(cached.inner().calls.load(Ordering::SeqCst), 0);
        assert_eq!(out[0].word, "Katherine");
        assert_eq!(out[0].start, Some(5));
    }

    #[test]
    fn without_a_store_it_is_a_pass_through() {
        let cached = CachedNer::new(Counting::new(), None);
        cached.classify("x");
        cached.classify("x");
        assert_eq!(cached.inner().calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn eviction_drops_the_oldest_and_respects_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cache.sqlite3");
        // A cap small enough that a few hundred entries overflow it.
        let st = SpanStore::open(&path, "fp", 64 * 1024).unwrap();
        let filler = "x".repeat(300);
        for i in 0..2000 {
            st.put(&format!("text-{i}-{filler}"), &[]);
        }
        st.evict_if_over();
        let survivors = st.len();
        assert!(
            survivors < 2000,
            "the cap must actually evict (still {survivors} entries)"
        );
        // The newest entry survives, the oldest is gone.
        assert!(st.get(&format!("text-1999-{filler}")).is_some());
        assert!(st.get(&format!("text-0-{filler}")).is_none());
    }

    #[test]
    fn a_corrupt_row_is_dropped_and_recomputed() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(tmp.path(), "fp");
        st.put("hello", &[]);
        {
            let conn = st.conn.lock().unwrap();
            conn.execute("UPDATE spans SET tokens = 'not json'", [])
                .unwrap();
        }
        assert!(
            st.get("hello").is_none(),
            "corrupt rows must read as a miss"
        );
        assert!(st.is_empty(), "and be deleted so they cannot rot in place");
    }

    #[test]
    fn tokens_round_trip_exactly() {
        let tokens = vec![
            NerToken {
                entity: "B-private_person".into(),
                word: "Kath".into(),
                start: Some(0),
                end: Some(4),
            },
            NerToken {
                entity: "E-private_person".into(),
                word: "##erine".into(),
                start: None,
                end: None,
            },
        ];
        let decoded = decode_tokens(&encode_tokens(&tokens)).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].entity, "B-private_person");
        assert_eq!(decoded[0].start, Some(0));
        assert_eq!(decoded[1].word, "##erine");
        assert_eq!(decoded[1].end, None);
    }
}
