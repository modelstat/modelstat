//! Pre-AI repo anchors on the outgoing batch: which repos to mine, when a
//! re-mine is warranted, and the small on-disk record that keeps one mine from
//! becoming a mine every cycle.
//!
//! Mining walks real git history, so it is gated four ways: only when the
//! device has not opted out (`MODELSTAT_ANCHORS=0`), at most once per repo per
//! daemon RUN (in memory), only when the repo's HEAD moved since the last mine
//! (`anchors.json`, beside `state.json`), and at most
//! [`caps::REPO_ANCHORS_COUNT_MAX`] repos per batch — the ones the batch was
//! most recently active in, since those are the repos whose AI-era work is
//! actually arriving.
//!
//! What ships is the repo's own public history (slug, PR numbers, shas,
//! timestamps, counts — see [`modelstat_parsers::git_anchors`]). What stays
//! here is the local bookkeeping: absolute repo paths live in `anchors.json`
//! and nowhere else, exactly like the transcript paths keying `state.json`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use modelstat_parsers::git::resolve_repo_root;
use modelstat_parsers::{head_sha, mine_repo_anchors, AnchorConfig, DEFAULT_CUTOFF};
use modelstat_wire::{caps, RawEvent, RepoAnchors};

/// Ceiling on the mining one batch may trigger. Each repo carries its own
/// budget inside the miner; this bounds the batch when several repos go stale
/// at once (a fresh install, where every repo is a first mine).
const BATCH_BUDGET: Duration = Duration::from_secs(60);

/// What a finished mine left behind for one repo, keyed by repo root.
///
/// `head_sha` is the whole point: history before a fixed cutoff cannot change
/// unless the repo itself was rewritten, so an unchanged HEAD means an
/// unchanged answer and the walk is skipped entirely. `mined_at` is kept so the
/// record says WHEN, the same pairing the server uses to dedupe re-mines.
#[derive(Debug, Serialize, Deserialize)]
struct MinedRecord {
    head_sha: String,
    mined_at: String,
}

/// Repo root → the last mine. `BTreeMap` so the file is stable across writes.
type MineLog = BTreeMap<String, MinedRecord>;

/// Whether this device mines anchors at all. `MODELSTAT_ANCHORS=0` (or `off` /
/// `false`) is a total opt-out — no git walk, no cache file, no wire field.
fn anchors_enabled() -> bool {
    !matches!(
        std::env::var("MODELSTAT_ANCHORS")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("0") | Some("off") | Some("false")
    )
}

/// The pre-AI cutoff for this device: `MODELSTAT_ANCHOR_CUTOFF` (ISO-8601) when
/// it is one, else [`DEFAULT_CUTOFF`].
///
/// A garbage value falls back and says so rather than being passed to git,
/// which would silently mine a window nobody chose (`--until=junk` is not an
/// error to git the way it is to a reader).
fn cutoff_from_env() -> String {
    let raw = std::env::var("MODELSTAT_ANCHOR_CUTOFF")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    match raw {
        None => DEFAULT_CUTOFF.to_string(),
        Some(v) if chrono::DateTime::parse_from_rfc3339(&v).is_ok() => v,
        Some(v) => {
            modelstat_log::log_warn!(
                "MODELSTAT_ANCHOR_CUTOFF={v} is not an ISO-8601 instant — \
                 mining pre-AI anchors before {DEFAULT_CUTOFF} instead"
            );
            DEFAULT_CUTOFF.to_string()
        }
    }
}

/// The per-device anchor miner: process-lifetime state (what has been mined
/// this run) over a persisted record (what was mined in earlier runs).
pub struct AnchorMiner {
    /// `~/.modelstat/anchors.json` — the per-repo mine log.
    path: PathBuf,
    cfg: AnchorConfig,
    /// Repo roots already resolved this RUN, mined or skipped. Keeps a repo to
    /// one decision per daemon process however many batches it appears in.
    seen: Mutex<HashSet<String>>,
}

impl AnchorMiner {
    /// The miner for this device, or `None` when mining is switched off.
    pub fn from_env(path: PathBuf) -> Option<Self> {
        anchors_enabled().then(|| AnchorMiner {
            path,
            cfg: AnchorConfig {
                cutoff: cutoff_from_env(),
                ..AnchorConfig::default()
            },
            seen: Mutex::new(HashSet::new()),
        })
    }

    /// Anchors for the repos this batch touched, newest-active first.
    ///
    /// `None` when there is nothing to say — never `Some(vec![])`, which would
    /// spend a wire field to state what an absent key already states, and would
    /// hand the server an "answer" for repos it must not draw a baseline for.
    pub fn anchors_for(&self, events: &[RawEvent]) -> Option<Vec<RepoAnchors>> {
        let deadline = Instant::now() + BATCH_BUDGET;
        let mut log = read_log(&self.path);
        let mut dirty = false;
        let mut out: Vec<RepoAnchors> = Vec::new();
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        for cwd in recent_cwds(events) {
            if out.len() >= caps::REPO_ANCHORS_COUNT_MAX || Instant::now() >= deadline {
                break;
            }
            // Worktrees collapse to the main repo, so a session run from an
            // ephemeral worktree mines (and caches) the repo it belongs to.
            let Some(root) = resolve_repo_root(Some(&cwd)) else {
                continue;
            };
            if !seen.insert(root.clone()) {
                continue;
            }
            let Some(head) = head_sha(&root) else {
                continue;
            };
            if log.get(&root).is_some_and(|m| m.head_sha == head) {
                continue;
            }
            let Some(mined) = mine_repo_anchors(&root, &self.cfg) else {
                continue;
            };
            // Recorded even when the mine found nothing: "this repo has no
            // pre-AI PR merges" is an answer, and re-walking its history every
            // run to rediscover it is the cost this record exists to avoid.
            log.insert(
                root,
                MinedRecord {
                    head_sha: head,
                    mined_at: mined.mined_at.clone(),
                },
            );
            dirty = true;
            if !mined.anchors.is_empty() {
                out.push(mined);
            }
        }
        drop(seen);
        if dirty {
            write_log(&self.path, &log);
        }
        (!out.is_empty()).then_some(out)
    }
}

/// The distinct working directories this batch saw, most recently active first.
/// Pure.
///
/// Recency is the selection rule because the cap is a cap: when a batch spans
/// more repos than may ride, the ones whose sessions just landed are the ones
/// whose costs the server is about to try to explain.
fn recent_cwds(events: &[RawEvent]) -> Vec<String> {
    // BTreeMap, so two cwds last touched in the same millisecond order
    // deterministically instead of by hash seed.
    let mut newest: BTreeMap<&str, i64> = BTreeMap::new();
    for e in events {
        let Some(cwd) = e.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) else {
            continue;
        };
        let ms = chrono::DateTime::parse_from_rfc3339(&e.ts)
            .map(|d| d.timestamp_millis())
            .unwrap_or(i64::MIN);
        let slot = newest.entry(cwd).or_insert(i64::MIN);
        *slot = (*slot).max(ms);
    }
    let mut ranked: Vec<(&str, i64)> = newest.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    ranked.into_iter().map(|(cwd, _)| cwd.to_string()).collect()
}

/// Best-effort read; a missing or corrupt log simply means everything re-mines.
fn read_log(path: &Path) -> MineLog {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Atomic write (tmp + rename), the shape the insights cache and `state.json`
/// use. Best-effort: a failed write costs one re-mine, never a batch.
fn write_log(path: &Path, log: &MineLog) {
    let Ok(text) = serde_json::to_string(log) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env this module reads is process-global; `cargo test` runs these on
    /// several threads at once. Poison-safe, like the other env locks here.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with_env<T>(pairs: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let _g = env_lock();
        for (k, v) in pairs {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let out = f();
        for (k, _) in pairs {
            std::env::remove_var(k);
        }
        out
    }

    fn ev(cwd: Option<&str>, ts: &str) -> RawEvent {
        RawEvent {
            content_bytes: None,
            source_event_id: format!("e:{ts}"),
            ts: ts.into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s1".into(),
            turn_index: None,
            parent_event_id: None,
            cwd: cwd.map(str::to_string),
            git: None,
            tokens: None,
            tokens_unmapped: BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    fn tmp_log(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "modelstat-anchorlog-{}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn nothing_mined_ships_no_key_at_all() {
        // `Some(vec![])` would tell the server "this batch's repos have no
        // pre-AI baseline", which is a claim. An absent key makes none.
        let path = tmp_log("empty");
        let _ = std::fs::remove_file(&path);
        let miner = with_env(&[("MODELSTAT_ANCHORS", None)], || {
            AnchorMiner::from_env(path.clone()).unwrap()
        });
        assert_eq!(miner.anchors_for(&[]), None);
        // A cwd under no git repo at all: resolvable to nothing, so mined nothing.
        let events = vec![ev(
            Some("/modelstat-no-such-root/inner"),
            "2026-07-16T10:00:00.000Z",
        )];
        assert_eq!(miner.anchors_for(&events), None);
        // And a mine that found nothing wrote no log to grow forever.
        assert!(!path.exists());
    }

    #[test]
    fn opting_out_builds_no_miner() {
        for value in ["0", "off", "false", " 0 "] {
            let built = with_env(&[("MODELSTAT_ANCHORS", Some(value))], || {
                AnchorMiner::from_env(tmp_log("off")).is_some()
            });
            assert!(!built, "MODELSTAT_ANCHORS={value} must disable mining");
        }
        let on = with_env(&[("MODELSTAT_ANCHORS", Some("1"))], || {
            AnchorMiner::from_env(tmp_log("on")).is_some()
        });
        assert!(on);
    }

    #[test]
    fn a_bad_cutoff_falls_back_instead_of_mining_a_window_nobody_chose() {
        let honored = with_env(
            &[("MODELSTAT_ANCHOR_CUTOFF", Some("2019-01-01T00:00:00Z"))],
            cutoff_from_env,
        );
        assert_eq!(honored, "2019-01-01T00:00:00Z");
        for bad in ["yesterday", "2019-01-01", ""] {
            let got = with_env(&[("MODELSTAT_ANCHOR_CUTOFF", Some(bad))], cutoff_from_env);
            assert_eq!(got, DEFAULT_CUTOFF, "{bad:?} must not become the cutoff");
        }
    }

    #[test]
    fn repos_rank_by_their_newest_activity_in_the_batch() {
        let events = vec![
            ev(Some("/w/old"), "2026-07-16T09:00:00.000Z"),
            ev(Some("/w/new"), "2026-07-16T10:00:00.000Z"),
            ev(Some("/w/old"), "2026-07-16T11:00:00.000Z"),
            ev(None, "2026-07-16T12:00:00.000Z"),
            ev(Some("   "), "2026-07-16T12:00:00.000Z"),
        ];
        // `/w/old` wins on its LATEST event, not its first or its count.
        assert_eq!(recent_cwds(&events), vec!["/w/old", "/w/new"]);
        assert!(recent_cwds(&[]).is_empty());
    }

    #[test]
    fn the_mine_log_round_trips_and_survives_corruption() {
        let path = tmp_log("roundtrip");
        let _ = std::fs::remove_file(&path);
        assert!(read_log(&path).is_empty());
        let mut log = MineLog::new();
        log.insert(
            "/w/repo".into(),
            MinedRecord {
                head_sha: "abc".into(),
                mined_at: "2026-07-16T10:00:00.000Z".into(),
            },
        );
        write_log(&path, &log);
        assert_eq!(read_log(&path)["/w/repo"].head_sha, "abc");
        // A half-written or hand-edited file re-mines rather than wedging.
        std::fs::write(&path, "{not json").unwrap();
        assert!(read_log(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
