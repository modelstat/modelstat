//! Per-session insights cache — a port of `apps/daemon/src/insights.ts`.
//!
//! After a session scan the daemon asks the server for that session's rolled-up
//! insights (tokens, $ assigned, taxonomy nodes, status) via the unified MCP
//! `session_insights` tool, and CACHES the payload under
//! `~/.modelstat/sessions/<sessionId>.json`. The always-on `modelstat statusline`
//! command reads ONLY this cache (never the network), so the status line is
//! instant + offline-tolerant. Server enrichment is async (tokens+cost in ~2s,
//! taxonomy follows), so a fresh session is briefly un-enriched; we short-poll a
//! small bounded number of times so the cache converges to a TERMINAL status.
//!
//! The `sessions_dir` + the network fetch are injected: daemon-main passes the
//! real `home_path("sessions")` + a `DeviceApi`-backed MCP fetcher, tests pass a
//! temp dir + a fake — so the poll/converge logic is unit-testable offline.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// One taxonomy node detected for a session (the chips the widget + statusline
/// render). Loose on purpose — a server-side field addition can't break the read.
#[derive(Debug, Clone, Deserialize)]
pub struct InsightTaxonomyNode {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub root_key: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub emoji: Option<String>,
}

/// Parsed `session_insights` payload as cached on disk. Optional/loose — the
/// authoritative schema is the server's; the statusline reads defensively.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionInsights {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub segments_pending: Option<u64>,
    #[serde(default)]
    pub segment_count: Option<u64>,
    #[serde(default)]
    pub tokens: Option<Value>,
    /// `string | number | null` on the wire — kept as raw JSON.
    #[serde(default)]
    pub cost_usd: Option<Value>,
    #[serde(default)]
    pub taxonomy_nodes: Option<Vec<InsightTaxonomyNode>>,
    /// Stamped by the daemon when it wrote the cache (not from the server).
    #[serde(default)]
    pub cached_at: Option<String>,
}

/// Bounded short-poll delays while the server is still working.
const POLL_DELAYS_MS: [u64; 4] = [1200, 2000, 3000, 4000];

/// The server has finished enriching this session.
pub const STATUS_READY: &str = "ready";
/// The server has never seen this session — there is nothing to wait for.
pub const STATUS_NOT_INGESTED: &str = "not_ingested";

/// The statuses that mean the server is DONE with this session — it has enriched
/// it, or it has nothing to enrich. Everything else, including a status this
/// build has never heard of, is read as still-working.
///
/// This is the honest direction of the test. Polling `while status == "analyzing"`
/// made every unknown status terminal, so the day the server adds `queued` or
/// `summarising` the fleet stops on the FIRST reply and caches a half-enriched
/// payload — a break that ships from the server and cannot be fixed there. The
/// wrong reading is bounded here: at worst four extra polls over ~10s.
const TERMINAL_STATUSES: [&str; 2] = [STATUS_READY, STATUS_NOT_INGESTED];

/// True while the server may still have work to do for this session.
fn still_working(status: Option<&str>) -> bool {
    match status {
        // No status at all (a fetch failure) — nothing to converge to.
        None => false,
        Some(s) => !TERMINAL_STATUSES.contains(&s),
    }
}

/// Percent-encode a session id for a safe path segment (matches the intent of
/// `encodeURIComponent` — a `/` can never escape the sessions dir). UUID ids
/// (the universe) pass through unchanged; only unusual bytes get `%XX`.
fn encode_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Absolute path to one session's cached insights under `sessions_dir`.
pub fn session_insights_path(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{}.json", encode_id(session_id)))
}

/// Read a session's cached insights SYNCHRONOUSLY — the statusline's only data
/// source. `None` when there's no cache (not scanned yet) or it's unreadable/
/// corrupt. Deliberately sync + swallowing: the statusline runs on every render
/// and must never block or throw. Port of `readCachedInsightsSync`.
pub fn read_cached_insights_sync(sessions_dir: &Path, session_id: &str) -> Option<SessionInsights> {
    let raw = std::fs::read_to_string(session_insights_path(sessions_dir, session_id)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The MCP `session_insights` fetch seam. `None` = not enrolled / call failed /
/// unexpected body (the caller treats it as "nothing to cache this round"). The
/// daemon backs this with an authed POST to `/v1/mcp/call`; tests use a fake.
/// Returns the RAW insights JSON so unknown server fields survive the cache.
pub trait SessionInsightsFetcher {
    async fn fetch(&self, session_ids: &[String], eager: bool) -> Option<Value>;
}

/// Atomically write a session's insights to the cache (tmp + rename), stamping
/// `cached_at`. Keeps the raw JSON so a server-side field addition survives.
/// Port of `cacheSessionInsights`.
pub fn cache_session_insights(
    sessions_dir: &Path,
    session_id: &str,
    insights: &Value,
    cached_at: &str,
) -> std::io::Result<()> {
    let path = session_insights_path(sessions_dir, session_id);
    let mut payload = insights.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "cached_at".to_string(),
            Value::String(cached_at.to_string()),
        );
    }
    std::fs::create_dir_all(sessions_dir)?;
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, serde_json::to_string(&payload)?)?;
    std::fs::rename(&tmp, &path)
}

/// The status string in an insights payload, if any.
fn status_of(insights: &Value) -> Option<&str> {
    insights.get("status").and_then(Value::as_str)
}

/// Fetch + cache one session chain's insights, eagerly prioritising the server's
/// enrichment, and short-poll a few times while the server may still be working
/// ([`still_working`]) so the cache converges to a terminal status. Every interim
/// result is cached too (the statusline shows progress). Best-effort throughout —
/// a write failure never sinks a scan. Keyed by the FIRST id
/// (Claude Code's live `session_id`). Port of
/// `refreshSessionInsights`. `now_iso` stamps `cached_at` (injected so callers
/// control the clock).
pub async fn refresh_session_insights<F: SessionInsightsFetcher>(
    fetcher: &F,
    session_ids: &[String],
    sessions_dir: &Path,
    now_iso: impl Fn() -> String,
) {
    let Some(cache_key) = session_ids.first() else {
        return;
    };
    let mut insights = fetcher.fetch(session_ids, true).await;
    if let Some(ref v) = insights {
        let _ = cache_session_insights(sessions_dir, cache_key, v, &now_iso());
    }
    for delay in POLL_DELAYS_MS {
        // Stop only on a status that explicitly says the server is finished.
        if !still_working(insights.as_ref().and_then(status_of)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(delay)).await;
        // Re-poll WITHOUT re-prioritising — the priority signal already fired.
        if let Some(next) = fetcher.fetch(session_ids, false).await {
            let _ = cache_session_insights(sessions_dir, cache_key, &next, &now_iso());
            insights = Some(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("modelstat-insights-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn cache_write_then_sync_read_round_trips_with_cached_at() {
        let dir = tmp_dir("rt");
        let payload = json!({ "status": "ready", "segment_count": 3, "cost_usd": "1.50" });
        cache_session_insights(&dir, "sess-1", &payload, "2026-07-16T10:00:00.000Z").unwrap();

        let read = read_cached_insights_sync(&dir, "sess-1").unwrap();
        assert_eq!(read.status, "ready");
        assert_eq!(read.segment_count, Some(3));
        assert_eq!(read.cached_at.as_deref(), Some("2026-07-16T10:00:00.000Z"));
        assert_eq!(read.cost_usd, Some(json!("1.50")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_of_a_missing_session_is_none() {
        let dir = tmp_dir("miss");
        assert!(read_cached_insights_sync(&dir, "nope").is_none());
    }

    // A fetcher that returns a scripted sequence of payloads (one per call).
    struct ScriptedFetcher {
        replies: Mutex<std::collections::VecDeque<Value>>,
        eager_calls: Mutex<u32>,
    }
    impl SessionInsightsFetcher for ScriptedFetcher {
        async fn fetch(&self, _ids: &[String], eager: bool) -> Option<Value> {
            if eager {
                *self.eager_calls.lock().unwrap() += 1;
            }
            self.replies.lock().unwrap().pop_front()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_short_polls_analyzing_until_ready() {
        let dir = tmp_dir("poll");
        // analyzing (eager) → analyzing → ready.
        let fetcher = ScriptedFetcher {
            replies: Mutex::new(
                [
                    json!({ "status": "analyzing", "segment_count": 0 }),
                    json!({ "status": "analyzing", "segment_count": 1 }),
                    json!({ "status": "ready", "segment_count": 2 }),
                ]
                .into_iter()
                .collect(),
            ),
            eager_calls: Mutex::new(0),
        };
        let ids = vec!["sess-1".to_string()];
        refresh_session_insights(&fetcher, &ids, &dir, || {
            "2026-07-16T10:00:00.000Z".to_string()
        })
        .await;

        // The cache converged to ready, and the priority signal fired exactly once.
        let read = read_cached_insights_sync(&dir, "sess-1").unwrap();
        assert_eq!(read.status, "ready");
        assert_eq!(read.segment_count, Some(2));
        assert_eq!(*fetcher.eager_calls.lock().unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_stops_early_when_first_reply_is_ready() {
        let dir = tmp_dir("ready");
        let fetcher = ScriptedFetcher {
            replies: Mutex::new(
                [json!({ "status": "ready", "segment_count": 5 })]
                    .into_iter()
                    .collect(),
            ),
            eager_calls: Mutex::new(0),
        };
        let ids = vec!["s".to_string()];
        refresh_session_insights(&fetcher, &ids, &dir, || "t".to_string()).await;
        // Only the eager call ran — no polling once ready.
        assert_eq!(*fetcher.eager_calls.lock().unwrap(), 1);
        assert_eq!(
            read_cached_insights_sync(&dir, "s").unwrap().status,
            "ready"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_status_this_build_has_never_heard_of_keeps_polling() {
        // The server may name a new in-progress phase at any time. Reading an
        // unknown status as terminal would stop the fleet on the first reply and
        // cache a half-enriched payload — a break shipped from the server that
        // the server could not then fix.
        let dir = tmp_dir("unknown");
        let fetcher = ScriptedFetcher {
            replies: Mutex::new(
                [
                    json!({ "status": "queued", "segment_count": 0 }),
                    json!({ "status": "summarising", "segment_count": 1 }),
                    json!({ "status": "ready", "segment_count": 3 }),
                ]
                .into_iter()
                .collect(),
            ),
            eager_calls: Mutex::new(0),
        };
        let ids = vec!["sess-x".to_string()];
        refresh_session_insights(&fetcher, &ids, &dir, || "t".to_string()).await;
        let read = read_cached_insights_sync(&dir, "sess-x").unwrap();
        assert_eq!(read.status, "ready");
        assert_eq!(read.segment_count, Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_explicit_terminal_markers_stop_the_poll() {
        assert!(!still_working(Some("ready")));
        assert!(!still_working(Some("not_ingested")));
        assert!(still_working(Some("analyzing")));
        assert!(still_working(Some("anything_new")));
        // No status at all means the fetch failed — nothing to converge to.
        assert!(!still_working(None));
    }
}
