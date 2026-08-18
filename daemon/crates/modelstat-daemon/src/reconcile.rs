//! Self-healing reconcile — bidirectional anti-entropy (spec
//! `specs/self-healing-ingest.md`).
//!
//! The server is authoritative for what's ingested; the local cursor is only a
//! fast-path that goes stale when the server loses data. So we reconcile against
//! the server with a digest tree keyed on EVENTS (the cheap layer both sides hold
//! without the summariser):
//!
//!   1. total — one number. Server holds everything ⇒ done (O(1) steady state).
//!   2. per-day — drill only into days the server is short on.
//!   3. per-session — within a short (and settled) day, find the exact sessions
//!      and invalidate just those files' cursors so the normal scan re-ships them.
//!
//! The local digest is built INCREMENTALLY (a per-file mtime→counts cache) so a
//! device with months of history reconciles in O(changed files). Everything the
//! pass does I/O with is an injected seam ([`BackfillDigest`], [`ReconcileStore`],
//! the `list_files` / `parse_counts` / `request_scan` closures) so the whole
//! digest-tree decision is unit-testable without a server or real transcripts.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::json;

/// day → session → event count (the cheap digest layer both sides hold).
pub type PerDaySession = BTreeMap<String, BTreeMap<String, i64>>;

/// One per-file digest cache entry: mtime-keyed, so an unchanged file is a hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestEntry {
    /// `stat().mtimeMs` at the time these counts were computed.
    pub mtime: f64,
    #[serde(rename = "perDaySession")]
    pub per_day_session: PerDaySession,
}

/// The reconcile cache: path → its digest entry (persisted in `state.json`).
pub type ReconcileCache = BTreeMap<String, DigestEntry>;

/// One re-ship backoff record for a `(day, session)` the server stayed short on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReshipRec {
    pub attempts: u32,
    #[serde(rename = "lastAt")]
    pub last_at: i64,
}

/// `(day, session)` key → its backoff record.
pub type ReshipState = BTreeMap<String, ReshipRec>;

/// Top of the reconcile tree: scope-wide total + per-day rollup.
#[derive(Debug, Clone, Deserialize)]
pub struct BackfillDays {
    pub total_events: i64,
    pub days: Vec<DayCount>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DayCount {
    pub day: String,
    pub events: i64,
}

/// Drill level: one day's per-session counts (fetched only for divergent days).
#[derive(Debug, Clone, Deserialize)]
pub struct BackfillDaySessions {
    #[allow(dead_code)]
    pub day: String,
    pub sessions: Vec<SessionCount>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionCount {
    pub session_id: String,
    pub events: i64,
}

/// What one reconcile pass observed. Mirrors TS `ReconcileOutcome`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    /// The server already holds everything local — no re-ship.
    pub in_sync: bool,
    pub local_events: i64,
    pub server_events: i64,
    /// Files re-parsed this pass (0 in steady state — everything cache-hit).
    pub files_parsed: usize,
    /// Divergent, settled days drilled into.
    pub days_checked: usize,
    /// Sessions the server was short on AND that cleared the grace band.
    pub sessions_short: usize,
    /// Sessions short but HELD back by the grace band (settle / skew / backoff) —
    /// counted so a deferral is observable, not silent.
    pub sessions_deferred: usize,
    /// Cursors invalidated so the next scan re-ships them.
    pub files_invalidated: usize,
}

// Grace band so a server that DEDUPES a re-shipped session doesn't make us
// re-ship forever — three guards, all erring toward loss-PROOF (see reconcile.ts).
const RESHIP_SETTLE_MS: i64 = 6 * 60 * 60_000; // events older than this should have settled
const RESHIP_MIN_DEFICIT: i64 = 2; // ignore a shortfall of ≤2 events (benign skew)
const RESHIP_BACKOFF_BASE_MS: i64 = 30 * 60_000; // first retry no sooner than ~30min
const RESHIP_BACKOFF_MAX_MS: i64 = 24 * 60 * 60_000; // never wait longer than a day

/// The server-side digest source (`GET /v1/backfill/digests`). `None` = the
/// digest is unreachable or the route degraded to the SPA-fallback — the pass is
/// a no-op and retries next time (never a crash).
// In-crate seam only (the daemon's HTTP client + tests implement it); no caller
// ever needs `Send` bounds on the futures, so plain `async fn` stays the
// clearer spelling.
#[allow(async_fn_in_trait)]
pub trait BackfillDigest {
    async fn fetch_days(&mut self) -> Option<BackfillDays>;
    async fn fetch_day_sessions(&mut self, day: &str) -> Option<BackfillDaySessions>;
}

/// The local reconcile state — cache + backoff bookkeeping + cursor invalidation.
/// The daemon backs this with `RuntimeState` (persisted after the pass); tests use
/// it directly (the impl below).
pub trait ReconcileStore {
    fn reconcile_cache(&self) -> ReconcileCache;
    fn set_reconcile_cache(&mut self, cache: ReconcileCache);
    fn reship_state(&self) -> ReshipState;
    fn set_reship_state(&mut self, state: ReshipState);
    /// Drop cursor entries for files not in `present` (GC to the current set).
    fn prune_cursors(&mut self, present: &BTreeSet<String>);
    /// Invalidate one file's cursor so the next scan re-parses + re-ships it.
    fn clear_cursor(&mut self, path: &str);
}

impl ReconcileStore for modelstat_ingest::RuntimeState {
    fn reconcile_cache(&self) -> ReconcileCache {
        serde_json::from_value(self.reconcile_cache.clone()).unwrap_or_default()
    }
    fn set_reconcile_cache(&mut self, cache: ReconcileCache) {
        self.reconcile_cache = serde_json::to_value(cache).unwrap_or_else(|_| json!({}));
    }
    fn reship_state(&self) -> ReshipState {
        serde_json::from_value(self.reship_state.clone()).unwrap_or_default()
    }
    fn set_reship_state(&mut self, state: ReshipState) {
        self.reship_state = serde_json::to_value(state).unwrap_or_else(|_| json!({}));
    }
    fn prune_cursors(&mut self, present: &BTreeSet<String>) {
        self.cursor.retain(|p, _| present.contains(p));
    }
    fn clear_cursor(&mut self, path: &str) {
        self.cursor.remove(path);
    }
}

/// UTC calendar day of a timestamp in epoch-ms — matches the server's `toDate`.
/// `""` when the value can't be represented (mirrors the TS `NaN` guard).
fn utc_day_from_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Run one reconcile pass. `None` when the server digest is unreachable (no-op;
/// the next pass retries). Port of `reconcileBackfill`.
///
/// Seams: `list_files` yields `(path, mtimeMs)` for every discovered transcript
/// (discover + stat); `parse_counts` parses a CHANGED file into its
/// `day→session→count` digest (folding `utcDay` over event timestamps);
/// `digest` fetches the server tree; `store` holds the cache/backoff/cursors;
/// `request_scan` kicks the scan loop when files are invalidated.
pub async fn reconcile_backfill<D, S, LF, PC, RS>(
    now_ms: i64,
    list_files: LF,
    mut parse_counts: PC,
    digest: &mut D,
    store: &mut S,
    request_scan: RS,
) -> Option<ReconcileOutcome>
where
    D: BackfillDigest,
    S: ReconcileStore,
    LF: Fn() -> Vec<(String, f64)>,
    PC: FnMut(&str) -> Option<PerDaySession>,
    RS: FnOnce(&str),
{
    // 1. Incrementally refresh the local digest cache — re-parse only files whose
    //    mtime changed; reuse cached counts for everything else.
    let files = list_files();
    let present: BTreeSet<String> = files.iter().map(|(p, _)| p.clone()).collect();
    let mut cache = store.reconcile_cache();
    let mut files_parsed = 0usize;
    for (path, mtime) in &files {
        if let Some(hit) = cache.get(path) {
            if hit.mtime == *mtime {
                continue; // unchanged — reuse cached counts
            }
        }
        if let Some(per_day_session) = parse_counts(path) {
            cache.insert(
                path.clone(),
                DigestEntry {
                    mtime: *mtime,
                    per_day_session,
                },
            );
            files_parsed += 1;
        }
        // A parse failure leaves any prior entry in place (its old mtime forces a
        // re-parse next pass) and never aborts the reconcile.
    }
    // GC: drop cache + cursor entries for files that no longer exist, so on-disk
    // state tracks the CURRENT file set rather than everything ever seen.
    //
    // ── INVARIANT: absence is LOCAL. It never travels. ──────────────────────
    // This is the one place in the daemon that learns a file it used to see is
    // gone, so it is the one place tempted to tell somebody. It must not.
    //
    // The local transcript is the SOURCE; the server is the DURABLE RECORD.
    // Agents prune their own history (Claude Code trims `~/.claude/projects`,
    // codex rollouts get cleaned up), disks are reimaged, worktrees are thrown
    // away — none of that is a statement that the work did not happen. A
    // session that reached the server stays there until a PERSON deletes it.
    //
    // So the two calls below are the complete allowed response to a vanished
    // file: forget our own bookkeeping about it. Do NOT grow this into a
    // deletion, a tombstone, a retirement, or a "files I no longer have" list
    // on any request. Note what the daemon can even say: it holds no HTTP
    // DELETE anywhere, and `IngestBatch` has no field that can express
    // "this is gone" — the property is structural today, and the cost of
    // giving that up is 116 sessions and 29.5% of measured work, which is what
    // the last supersession bug took (core#701, and see
    // `flush::ScanGeneration::claims`).
    //
    // The knock-on effects are already the safe direction, deliberately:
    //   * a pruned cursor makes the file re-ship if it ever returns, never
    //     un-ship (the server upserts by `source_event_id`);
    //   * a dropped cache entry LOWERS `local_events`, so the in-sync test
    //     below is less likely to re-ship, not more — the server's fuller
    //     copy simply wins;
    //   * a forced re-scan (a pipeline-aspect bump) that no longer finds the
    //     file counts it as no work left rather than work outstanding, so it
    //     cannot pin a re-scan open forever
    //     (`processing_version::rescans_in_progress`).
    cache.retain(|p, _| present.contains(p));
    store.set_reconcile_cache(cache.clone());
    store.prune_cursors(&present);

    // 2. Roll the cache up → local per-day totals, per-(day,session) counts, and
    //    the file(s) each (day,session) lives in (for precise re-ship).
    let mut local_day: BTreeMap<String, i64> = BTreeMap::new();
    let mut local_day_session: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    let mut files_of: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut local_events = 0i64;
    for (path, entry) in &cache {
        for (day, sessions) in &entry.per_day_session {
            let ds = local_day_session.entry(day.clone()).or_default();
            for (sid, n) in sessions {
                local_events += n;
                *local_day.entry(day.clone()).or_default() += n;
                *ds.entry(sid.clone()).or_default() += n;
                files_of
                    .entry((day.clone(), sid.clone()))
                    .or_default()
                    .insert(path.clone());
            }
        }
    }

    // 3. Top of the tree: totals. The daemon only ADDS and the digest is
    //    device-scoped, so `server >= local` ⇒ nothing to push.
    let server_days = digest.fetch_days().await?;
    let mut outcome = ReconcileOutcome {
        in_sync: true,
        local_events,
        server_events: server_days.total_events,
        files_parsed,
        ..Default::default()
    };
    if local_events <= server_days.total_events {
        return Some(outcome);
    }

    // Settle cutoff: only reconcile calendar days STRICTLY OLDER than this — recent
    // days legitimately lag (async ingest), so a shortfall there is in-flight data.
    let cutoff_day = utc_day_from_ms(now_ms - RESHIP_SETTLE_MS);

    // 4. Drill only into days the server is short on AND old enough to have settled.
    let server_day_map: BTreeMap<String, i64> = server_days
        .days
        .into_iter()
        .map(|d| (d.day, d.events))
        .collect();
    let mut reship = store.reship_state();
    let mut seen_keys: BTreeSet<String> = BTreeSet::new();
    let mut files_to_reship: BTreeSet<String> = BTreeSet::new();
    let mut sessions_short = 0usize;
    let mut sessions_deferred = 0usize;
    let mut days_checked = 0usize;
    for (day, local_count) in &local_day {
        if *local_count <= *server_day_map.get(day).unwrap_or(&0) {
            continue; // day in sync
        }
        if day.as_str() >= cutoff_day.as_str() {
            // Too recent to trust (still settling) — defer the whole day. Count its
            // sessions as deferred so the deferral is observable.
            sessions_deferred += local_day_session.get(day).map(BTreeMap::len).unwrap_or(0);
            continue;
        }
        days_checked += 1;
        let server_have: BTreeMap<String, i64> = digest
            .fetch_day_sessions(day)
            .await
            .map(|s| {
                s.sessions
                    .into_iter()
                    .map(|x| (x.session_id, x.events))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(sessions) = local_day_session.get(day) {
            for (sid, n) in sessions {
                let key = format!("{day}\0{sid}");
                seen_keys.insert(key.clone());
                let deficit = n - server_have.get(sid).unwrap_or(&0);
                if deficit <= 0 {
                    reship.remove(&key); // in sync — drop stale backoff bookkeeping
                    continue;
                }
                // Guard 2: ignore a tiny shortfall (benign skew, not loss).
                if deficit <= RESHIP_MIN_DEFICIT {
                    sessions_deferred += 1;
                    continue;
                }
                // Guard 3: exponential backoff once we've already re-shipped this
                // (day,session) and the server is STILL short (it deduped). First
                // sight → no record → re-ships now; later → 30min·2^(attempts-1) (cap 24h).
                if let Some(rec) = reship.get(&key) {
                    if rec.attempts > 0 {
                        let shift = (rec.attempts - 1).min(32);
                        let wait = RESHIP_BACKOFF_BASE_MS
                            .saturating_mul(1i64 << shift)
                            .min(RESHIP_BACKOFF_MAX_MS);
                        if now_ms - rec.last_at < wait {
                            sessions_deferred += 1;
                            continue; // still inside the backoff window
                        }
                    }
                }
                sessions_short += 1;
                let attempts = reship.get(&key).map(|r| r.attempts).unwrap_or(0) + 1;
                reship.insert(
                    key.clone(),
                    ReshipRec {
                        attempts,
                        last_at: now_ms,
                    },
                );
                if let Some(fs) = files_of.get(&(day.clone(), sid.clone())) {
                    for f in fs {
                        files_to_reship.insert(f.clone());
                    }
                }
            }
        }
    }
    // GC: drop backoff records for (day,session) pairs no longer seen as short or
    // no longer held locally, so the map tracks the current short set.
    reship.retain(|k, _| seen_keys.contains(k));
    store.set_reship_state(reship);

    outcome.days_checked = days_checked;
    outcome.sessions_deferred = sessions_deferred;
    if files_to_reship.is_empty() {
        return Some(outcome);
    }
    for f in &files_to_reship {
        store.clear_cursor(f);
    }
    modelstat_log::log_warn!(
        "self-heal: server short {sessions_short} session(s) across {days_checked} \
         day(s) ({sessions_deferred} deferred by grace band); re-shipping {} file(s) from local logs",
        files_to_reship.len()
    );
    request_scan("self-heal");
    outcome.in_sync = false;
    outcome.sessions_short = sessions_short;
    outcome.files_invalidated = files_to_reship.len();
    Some(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_ingest::state::FileCursor;
    use modelstat_ingest::RuntimeState;

    // A server digest fake: a fixed total + per-day rollup, and a per-day session
    // map queried on drill-down.
    struct FakeDigest {
        days: Option<BackfillDays>,
        sessions: BTreeMap<String, Vec<(String, i64)>>, // day → [(session, events)]
        day_fetches: Vec<String>,
    }
    impl BackfillDigest for FakeDigest {
        async fn fetch_days(&mut self) -> Option<BackfillDays> {
            self.days.clone()
        }
        async fn fetch_day_sessions(&mut self, day: &str) -> Option<BackfillDaySessions> {
            self.day_fetches.push(day.to_string());
            self.sessions.get(day).map(|list| BackfillDaySessions {
                day: day.to_string(),
                sessions: list
                    .iter()
                    .map(|(s, e)| SessionCount {
                        session_id: s.clone(),
                        events: *e,
                    })
                    .collect(),
            })
        }
    }

    fn server(total: i64, days: &[(&str, i64)]) -> Option<BackfillDays> {
        Some(BackfillDays {
            total_events: total,
            days: days
                .iter()
                .map(|(d, e)| DayCount {
                    day: (*d).into(),
                    events: *e,
                })
                .collect(),
        })
    }

    fn pds(day: &str, session: &str, n: i64) -> PerDaySession {
        let mut m = PerDaySession::new();
        m.entry(day.into()).or_default().insert(session.into(), n);
        m
    }

    // 2026-07-16T12:00:00Z in epoch-ms; cutoff (−6h) lands on "2026-07-16", so
    // "2026-07-10" is an OLD (settled) day and "2026-07-16" is deferred.
    fn now() -> i64 {
        chrono::DateTime::parse_from_rfc3339("2026-07-16T12:00:00Z")
            .unwrap()
            .timestamp_millis()
    }

    #[tokio::test]
    async fn in_sync_when_server_holds_everything() {
        let mut digest = FakeDigest {
            days: server(100, &[("2026-07-10", 100)]),
            sessions: BTreeMap::new(),
            day_fetches: Vec::new(),
        };
        let mut store = RuntimeState::default();
        let files = vec![("/a.jsonl".to_string(), 1.0f64)];
        let mut scanned = false;
        let out = reconcile_backfill(
            now(),
            || files.clone(),
            |_p| Some(pds("2026-07-10", "s1", 50)), // local 50 ≤ server 100
            &mut digest,
            &mut store,
            |_r| scanned = true,
        )
        .await
        .unwrap();

        assert!(out.in_sync);
        assert_eq!(out.local_events, 50);
        assert_eq!(out.server_events, 100);
        assert_eq!(out.files_invalidated, 0);
        assert!(!scanned);
        // No day drill-down when the top-level total already covers local.
        assert!(digest.day_fetches.is_empty());
    }

    #[tokio::test]
    async fn re_ships_a_genuinely_short_settled_session() {
        // Local has 50 events for an OLD day; server total is short (10) and the
        // day's session digest is short → re-ship.
        let mut digest = FakeDigest {
            days: server(10, &[("2026-07-10", 10)]),
            sessions: BTreeMap::from([("2026-07-10".to_string(), vec![("s1".to_string(), 10i64)])]),
            day_fetches: Vec::new(),
        };
        let mut store = RuntimeState::default();
        // Seed a cursor so we can prove it gets invalidated.
        store.cursor.insert(
            "/a.jsonl".into(),
            FileCursor {
                shipped_through_ms: None,
                size: 1,
                mtime: 1,
                tail_hash: "x".into(),
            },
        );
        let files = vec![("/a.jsonl".to_string(), 1.0f64)];
        let mut scan_reason = String::new();
        let out = reconcile_backfill(
            now(),
            || files.clone(),
            |_p| Some(pds("2026-07-10", "s1", 50)),
            &mut digest,
            &mut store,
            |r| scan_reason = r.to_string(),
        )
        .await
        .unwrap();

        assert!(!out.in_sync);
        assert_eq!(out.days_checked, 1);
        assert_eq!(out.sessions_short, 1);
        assert_eq!(out.files_invalidated, 1);
        assert_eq!(scan_reason, "self-heal");
        // Cursor cleared so the next scan re-ships the file.
        assert!(!store.cursor.contains_key("/a.jsonl"));
        // A backoff record was written for the re-shipped (day,session).
        assert_eq!(
            store.reship_state().get("2026-07-10\0s1").unwrap().attempts,
            1
        );
    }

    #[tokio::test]
    async fn defers_a_recent_unsettled_day() {
        // Local short on TODAY (within the 6h settle window) → deferred, no drill.
        let mut digest = FakeDigest {
            days: server(0, &[]),
            sessions: BTreeMap::new(),
            day_fetches: Vec::new(),
        };
        let mut store = RuntimeState::default();
        let files = vec![("/a.jsonl".to_string(), 1.0f64)];
        let out = reconcile_backfill(
            now(),
            || files.clone(),
            |_p| Some(pds("2026-07-16", "s1", 50)), // today
            &mut digest,
            &mut store,
            |_r| {},
        )
        .await
        .unwrap();

        assert!(out.in_sync); // no files invalidated
        assert_eq!(out.sessions_deferred, 1);
        assert_eq!(out.days_checked, 0);
        assert!(digest.day_fetches.is_empty()); // recent day never drilled
    }

    #[tokio::test]
    async fn ignores_a_sub_threshold_deficit() {
        // Old day, but the shortfall is ≤ RESHIP_MIN_DEFICIT (benign skew).
        let mut digest = FakeDigest {
            days: server(48, &[("2026-07-10", 48)]),
            sessions: BTreeMap::from([("2026-07-10".to_string(), vec![("s1".to_string(), 48i64)])]),
            day_fetches: Vec::new(),
        };
        let mut store = RuntimeState::default();
        let files = vec![("/a.jsonl".to_string(), 1.0f64)];
        let out = reconcile_backfill(
            now(),
            || files.clone(),
            |_p| Some(pds("2026-07-10", "s1", 50)), // deficit 2 == MIN → deferred
            &mut digest,
            &mut store,
            |_r| {},
        )
        .await
        .unwrap();

        assert_eq!(out.days_checked, 1);
        assert_eq!(out.sessions_short, 0);
        assert_eq!(out.sessions_deferred, 1);
        assert_eq!(out.files_invalidated, 0);
    }

    #[tokio::test]
    async fn backoff_defers_a_recently_reshipped_session() {
        // A (day,session) already re-shipped (attempts=1) 5min ago; the backoff
        // window (~30min) hasn't elapsed → deferred, not re-shipped again.
        let mut digest = FakeDigest {
            days: server(10, &[("2026-07-10", 10)]),
            sessions: BTreeMap::from([("2026-07-10".to_string(), vec![("s1".to_string(), 10i64)])]),
            day_fetches: Vec::new(),
        };
        let mut store = RuntimeState::default();
        let mut seed = ReshipState::new();
        seed.insert(
            "2026-07-10\0s1".into(),
            ReshipRec {
                attempts: 1,
                last_at: now() - 5 * 60_000, // 5min ago
            },
        );
        store.set_reship_state(seed);
        let files = vec![("/a.jsonl".to_string(), 1.0f64)];
        let mut scanned = false;
        let out = reconcile_backfill(
            now(),
            || files.clone(),
            |_p| Some(pds("2026-07-10", "s1", 50)),
            &mut digest,
            &mut store,
            |_r| scanned = true,
        )
        .await
        .unwrap();

        assert_eq!(out.sessions_short, 0);
        assert_eq!(out.sessions_deferred, 1);
        assert_eq!(out.files_invalidated, 0);
        assert!(!scanned);
    }

    #[tokio::test]
    async fn cache_hit_skips_reparse_and_gc_drops_vanished_files() {
        // Pre-seed the cache for /a.jsonl at mtime 1.0 → a cache hit (parse_counts
        // must NOT run for it). /gone.jsonl is in the cache but not present → GC'd.
        let mut store = RuntimeState::default();
        let mut seeded = ReconcileCache::new();
        seeded.insert(
            "/a.jsonl".into(),
            DigestEntry {
                mtime: 1.0,
                per_day_session: pds("2026-07-10", "s1", 7),
            },
        );
        seeded.insert(
            "/gone.jsonl".into(),
            DigestEntry {
                mtime: 1.0,
                per_day_session: pds("2026-07-10", "sX", 999),
            },
        );
        store.set_reconcile_cache(seeded);
        // A stale cursor for the vanished file — prune_cursors must drop it.
        store.cursor.insert(
            "/gone.jsonl".into(),
            FileCursor {
                shipped_through_ms: None,
                size: 1,
                mtime: 1,
                tail_hash: "x".into(),
            },
        );

        let mut digest = FakeDigest {
            days: server(100, &[("2026-07-10", 100)]), // server ahead → in_sync
            sessions: BTreeMap::new(),
            day_fetches: Vec::new(),
        };
        let files = vec![("/a.jsonl".to_string(), 1.0f64)]; // /gone.jsonl absent now
        let mut parse_calls = 0;
        let out = reconcile_backfill(
            now(),
            || files.clone(),
            |_p| {
                parse_calls += 1;
                Some(pds("2026-07-10", "s1", 7))
            },
            &mut digest,
            &mut store,
            |_r| {},
        )
        .await
        .unwrap();

        assert_eq!(parse_calls, 0); // cache hit — no re-parse
                                    // local_events counts only the surviving /a.jsonl (7), NOT the GC'd 999.
        assert_eq!(out.local_events, 7);
        assert!(!store.reconcile_cache().contains_key("/gone.jsonl"));
        assert!(!store.cursor.contains_key("/gone.jsonl")); // cursor pruned
    }

    #[tokio::test]
    async fn unreachable_digest_is_a_noop() {
        let mut digest = FakeDigest {
            days: None, // server unreachable / SPA-fallback
            sessions: BTreeMap::new(),
            day_fetches: Vec::new(),
        };
        let mut store = RuntimeState::default();
        let files = vec![("/a.jsonl".to_string(), 1.0f64)];
        let out = reconcile_backfill(
            now(),
            || files.clone(),
            |_p| Some(pds("2026-07-10", "s1", 50)),
            &mut digest,
            &mut store,
            |_r| {},
        )
        .await;
        assert!(out.is_none());
    }
}
