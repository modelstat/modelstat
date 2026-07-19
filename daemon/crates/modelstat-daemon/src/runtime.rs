//! Daemon-main runtime wiring — the closures + scan wrappers + the top-level
//! `run` loop that compose every M4 primitive into the live collector process.
//!
//! This file grows the daemon-main port (apps/daemon/src/daemon.ts). It starts
//! with the two remaining scan-loop closures whose construction needs concrete
//! types (`correct_events` owns a real `GitResolver`; `extract_links` is concrete
//! over `SummarizerClient` so its boxed future is `Send` — a generic engine
//! can't satisfy that under async-fn-in-trait). The boot sequence + async event
//! loop + heartbeat/last-status/shutdown land next.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Mutex as TokioMutex;

use modelstat_ingest::state::{load_state, save_state, FileCursor};
use modelstat_ingest::{home_path, Config, DeviceApi, RuntimeState};
use modelstat_parsers::git::resolve_repo_root;
use modelstat_parsers::{quick_checksum, GitResolver, RealGitEnrichment};
use modelstat_pipeline::passes::link_extract;
use modelstat_pipeline::{LinkExtractor, ResilientSummarizer};
use modelstat_receiver::FileQueueStore;
use modelstat_sumclient::SummarizerClient;
use modelstat_wire::RawEvent;

use crate::authoritative_git::resolve_authoritative_git;
use crate::discover_jobs::{
    discover_jobs, order_jobs_newest_first, parse_job_streaming, ParserKind, ScanJob,
};
use crate::engine::{build_embedder, build_ner, engine_base_url, DaemonEmbedder, DaemonNer};
use crate::insights::{refresh_session_insights, SessionInsightsFetcher};
use crate::scan::{
    run_scan_over_jobs, CursorStore, RunScanOptions, ScanObserver, ScanTallies, MAX_FILES_PER_SCAN,
};
use crate::status::{Phase, Status};

/// The scan loop's `correct_events` seam, backed by a real cwd-cached
/// `GitResolver`: rewrites each event's repo identity to the AUTHORITATIVE on-disk
/// remote before segmentation. Owns its own resolver (kept separate from the
/// metadata git enrichment — two caches, correctness-neutral). Port of the
/// daemon's `resolveAuthoritativeGit` wiring.
pub fn make_correct_events() -> impl FnMut(Vec<RawEvent>) -> Vec<RawEvent> {
    let mut resolver = GitResolver::new();
    move |events: Vec<RawEvent>| {
        resolve_authoritative_git(
            &events,
            |cwd| resolver.resolve(Some(cwd)),
            |cwd| resolve_repo_root(Some(cwd)),
        )
    }
}

/// The session-metadata `extract_links` seam — the model call that mines
/// code-collaboration references (PR/issue URLs, `org/repo#123`, ticket keys)
/// from a session's redacted abstracts. CONCRETE over `SummarizerClient` (not a
/// generic `S: Summarizer`) so the boxed future is provably `Send`; best-effort
/// (a `None` reply just leaves the deterministic reference channels standing).
pub fn make_extract_links(engine: &SummarizerClient) -> Box<LinkExtractor<'_>> {
    Box::new(move |abstracts: Vec<String>| -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        Box::pin(async move { link_extract(engine, &abstracts).await })
    })
}

/// ISO-8601 UTC now with millisecond precision + `Z`, byte-identical to JS
/// `new Date().toISOString()` — the format every timestamp the daemon stamps
/// (`last_event_at`, `cached_at`, `written_at`) uses.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Max bytes read from any referenced script before summarising (§ enrich).
/// Scripts are small; this bounds memory + model input.
const MAX_SCRIPT_READ_BYTES: usize = 64 * 1024;

/// The scan's `read_file` seam: read a referenced script file, capped, lossily
/// decoded (a non-UTF8 file still yields something to summarise, or is skipped
/// upstream). `None` when unreadable.
fn read_capped(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let end = bytes.len().min(MAX_SCRIPT_READ_BYTES);
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

/// The shared, process-lifetime handles every daemon subsystem reads. All heavy
/// state is already behind `Arc`/`Mutex`, so the scan runner, heartbeat, SDK
/// drain, reconcile, and watcher each hold a cheap `Arc<Daemon>` clone.
pub struct Daemon {
    pub config: Arc<Config>,
    pub api: Arc<DeviceApi>,
    pub resilient: Arc<ResilientSummarizer<SummarizerClient>>,
    pub embedder: Arc<DaemonEmbedder>,
    pub ner: Arc<DaemonNer>,
    /// Cursors + segments-sent + reconcile caches. An **async** mutex because a
    /// scan holds it across awaits (advancing cursors as batches commit);
    /// reconcile + processing-version take it too, so those serialise against a
    /// live scan instead of racing its cursor writes.
    pub state: Arc<TokioMutex<RuntimeState>>,
    /// Phase / progress / stats — a **std** mutex, only ever held briefly (never
    /// across an await), fed by the scan callbacks + read by the heartbeat.
    pub status: Arc<StdMutex<Status>>,
    /// The durable SDK ingest queue the loopback receiver writes + the drain reads.
    pub queue: Arc<FileQueueStore>,
    pub device_id: String,
    pub machine_id: String,
    /// The install-time summariser mode, resolved ONCE (a change bounces the
    /// service): `local` / `self-hosted` / `cloud`.
    pub mode: String,
    /// Auto-update dedup (§13): the `(verdict, target)` keys already acted on this
    /// process, so a heartbeat every 10s never stacks a second self-update.
    pub handled_updates: Arc<StdMutex<std::collections::HashSet<String>>>,
}

impl Daemon {
    /// Construct the shared handles from config: load persisted state, build the
    /// engine client for the active mode, and load the on-device models (real
    /// candle or fail-safe). Starts NO task — [`run`] owns the loop.
    pub fn build(config: Arc<Config>, device_id: String, machine_id: String) -> Arc<Self> {
        let mode = config.summarizer_mode();
        let engine = SummarizerClient::new(engine_base_url(&config));
        Arc::new(Daemon {
            api: Arc::new(DeviceApi::new(config.clone())),
            resilient: Arc::new(ResilientSummarizer::new(engine)),
            embedder: Arc::new(build_embedder()),
            ner: Arc::new(build_ner()),
            state: Arc::new(TokioMutex::new(load_state())),
            status: Arc::new(StdMutex::new(Status::default())),
            queue: Arc::new(FileQueueStore::new(home_path("queue.json"))),
            device_id,
            machine_id,
            mode,
            config,
            handled_updates: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        })
    }

    /// Briefly lock the live status to mutate it (never held across an await).
    /// Poison-safe: a panicked holder releases the lock rather than wedge the
    /// daemon.
    pub fn with_status<R>(&self, f: impl FnOnce(&mut Status) -> R) -> R {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s)
    }
}

/// Feeds scan progress into the live [`Status`] — the heartbeat + last-status
/// mirror read it, so the tray shows continuous activity through a long
/// summarise pass. Port of the `scanAll`/`scanSession` callbacks.
struct StatusObserver<'a> {
    status: &'a StdMutex<Status>,
}
impl StatusObserver<'_> {
    fn with<R>(&self, f: impl FnOnce(&mut Status) -> R) -> R {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s)
    }
}
impl ScanObserver for StatusObserver<'_> {
    fn on_file(&mut self, path: &str, index: usize, total: usize) {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        self.with(|s| {
            s.set_progress(index as u64 + 1, total as u64);
            s.set_message(format!("Scanning {}/{}: {name}", index + 1, total));
        });
    }
    fn on_upload(&mut self, _events: usize, segments: usize) {
        self.with(|s| {
            s.set_phase(Phase::Uploading, format!("Uploading {segments} segments"));
            s.set_stat("segments_sending", json!(segments));
        });
    }
    fn on_uploaded(&mut self, events: usize, _segments: usize) {
        let iso = now_iso();
        self.with(|s| {
            s.bump_stat("events_uploaded", events as u64);
            s.bump_stat("batches_uploaded", 1);
            s.set_stat("segments_sending", json!(0));
            s.note_event_at(iso);
        });
    }
}

/// The MCP-backed [`SessionInsightsFetcher`]: calls the server's unified
/// `session_insights` tool at `/v1/mcp/call` and returns the insights JSON the
/// tool's first text block carries. Owns a cheap [`DeviceApi`] handle. Port of
/// `fetchSessionInsights`.
struct McpInsightsFetcher {
    api: DeviceApi,
}
impl SessionInsightsFetcher for McpInsightsFetcher {
    async fn fetch(&self, session_ids: &[String], eager: bool) -> Option<Value> {
        if session_ids.is_empty() {
            return None;
        }
        let url = format!("{}/v1/mcp/call", self.api.config().api_url());
        let body = json!({
            "name": "session_insights",
            "arguments": { "session_ids": session_ids, "eager": eager },
        });
        let resp = self.api.post_json(&url, &body).await?;
        if resp.get("isError").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        // The MCP envelope carries the insights JSON as the first text block.
        let content = resp.get("content")?.as_array()?;
        let text = content
            .iter()
            .find(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .or_else(|| content.first())?
            .get("text")?
            .as_str()?;
        serde_json::from_str::<Value>(text).ok()
    }
}

/// Which parser an ad-hoc (control-endpoint) transcript path needs — the same
/// dir-shape rule `discover_jobs` uses, for a `--file` scan of a path discovery
/// hasn't enumerated yet (a brand-new file mid-write). Port of `parserForFile`.
fn kind_for_path(path: &str) -> ParserKind {
    if path.contains("/.codex/sessions/") {
        ParserKind::Codex
    } else if path.contains("/.pi/agent/sessions/") {
        ParserKind::Pi
    } else {
        ParserKind::ClaudeCode
    }
}

/// A discovered transcript path back to its session id — Claude Code's
/// `<uuid>.jsonl`, Codex's `rollout-…-<uuid>.jsonl`, or a pi session path. `None`
/// when none match (so it never collides with a real id). Port of
/// `sessionIdForPath`.
fn session_id_for_path(path: &str) -> Option<String> {
    if path.contains("/.pi/agent/sessions/") {
        return modelstat_parsers::pi::derive_session_id_from_pi_path(path);
    }
    modelstat_parsers::claude_code::derive_session_id_from_filename(path)
        .or_else(|| modelstat_parsers::codex::derive_session_id_from_rollout_path(path))
}

/// Resolve the eager-scan job set (port of `scanSession`'s target logic):
///   - an explicit `file` → its discovered job, else an ad-hoc one (so a
///     brand-new file mid-write still scans);
///   - else `session_ids` → the discovered jobs whose path maps to a wanted id;
///   - else → the single newest transcript (a bare invocation is still useful).
fn eager_target_jobs(session_ids: &[String], file: Option<&str>) -> Vec<ScanJob> {
    let all = discover_jobs();
    if let Some(f) = file {
        return match all.iter().find(|j| j.path == f) {
            Some(j) => vec![j.clone()],
            None => vec![ScanJob {
                path: f.to_string(),
                kind: kind_for_path(f),
            }],
        };
    }
    if !session_ids.is_empty() {
        let wanted: std::collections::BTreeSet<&str> =
            session_ids.iter().map(String::as_str).collect();
        return all
            .into_iter()
            .filter(|j| session_id_for_path(&j.path).is_some_and(|sid| wanted.contains(sid.as_str())))
            .collect();
    }
    order_jobs_newest_first(all).into_iter().take(1).collect()
}

/// Run ONE scan pass over the given ordered jobs, wiring every real adapter
/// (engine client, device uploader, git resolvers, model enrichment) to the pure
/// [`run_scan_over_jobs`] loop, then persisting advanced cursors + the
/// segments-sent lifetime total. Shared by the incremental [`run_scan_cycle`] and
/// the forced [`scan_session`]. The whole scan holds the state lock (cursors
/// mutate across awaits) — an `await`-safe tokio mutex, so reconcile +
/// processing-version serialise against it.
async fn execute_scan(daemon: &Daemon, ordered: Vec<ScanJob>, opts: RunScanOptions) -> ScanTallies {
    let device_id = daemon.device_id.as_str();
    let daemon_version = daemon.config.version();

    // The link-extraction seam borrows the concrete engine so its boxed future is
    // provably `Send` (a generic engine can't satisfy that under async-fn-in-trait).
    let extractor = make_extract_links(daemon.resilient.engine());
    let mut correct = make_correct_events();
    let mut git = RealGitEnrichment::new();
    // A cheap uploader handle (shares the HTTP pool) — the BatchUploader trait
    // wants `&mut self`, but DeviceApi's methods are all `&self`, so an owned
    // clone satisfies the borrow without blocking the shared `Arc<DeviceApi>`.
    let mut uploader = (*daemon.api).clone();
    let mut observer = StatusObserver {
        status: &daemon.status,
    };
    let exists = |p: &str| std::path::Path::new(p).exists();
    let read_file = |p: &str| read_capped(p);
    // The streaming parse seam runs on a spawn-blocking thread (a clone per file),
    // so it owns its device id + is Send/Sync/Clone/'static.
    let device_id_owned = daemon.device_id.clone();
    let parse = move |job: &ScanJob, emit: &mut dyn FnMut(Vec<RawEvent>)| {
        parse_job_streaming(&device_id_owned, job, emit)
    };
    let checksum = |path: &str| {
        quick_checksum(path).ok().map(|c| FileCursor {
            size: c.size,
            mtime: c.mtime as i64,
            tail_hash: c.tail_hash,
        })
    };

    let mut guard = daemon.state.lock().await;
    let tallies = run_scan_over_jobs(
        ordered,
        device_id,
        daemon_version,
        &daemon.mode,
        opts,
        &daemon.resilient,
        &*daemon.embedder,
        &*daemon.ner,
        &mut git,
        Some(&*extractor),
        parse,
        checksum,
        &mut correct,
        &exists,
        &read_file,
        &mut uploader,
        &mut *guard as &mut (dyn CursorStore + Send),
        &mut observer,
    )
    .await;

    // Fold the segments-sent lifetime total + persist the advanced cursors once,
    // atomically. A crash after this just re-sends idempotently from the cursor.
    if tallies.segments_uploaded > 0 {
        guard.segments_sent += tallies.segments_uploaded as i64;
    }
    let segments_sent = guard.segments_sent;
    let _ = save_state(&guard);
    drop(guard);

    daemon.with_status(|s| {
        s.bump_stat("files_scanned", tallies.files_scanned as u64);
        s.bump_stat("files_unchanged", tallies.files_unchanged as u64);
        s.set_stat("segments_sent", json!(segments_sent));
        s.set_stat("segments_sending", json!(0));
    });
    tallies
}

/// The incremental scan (cap 12, newest-first): the single-flight runner's task.
/// Drains a cold-start backlog over quick successive cap-12 passes *within one
/// coalesced run* (memory stays near one batch), rather than stacking concurrent
/// scans. A hold (engine/summariser down) stops the run loudly — the next trigger
/// (watcher / 5-min backstop) retries from exactly where it held. Port of
/// `runScanCycle` + its `morePending` re-trigger, inlined as a bounded loop.
pub async fn run_scan_cycle(daemon: Arc<Daemon>, reason: String) {
    daemon.with_status(|s| {
        s.set_phase(Phase::Scanning, format!("Scanning local JSONL ({reason})"));
        s.set_progress(0, 0);
    });
    let opts = RunScanOptions {
        max_files: Some(MAX_FILES_PER_SCAN),
        force_read_all: false,
    };
    loop {
        let jobs = order_jobs_newest_first(discover_jobs());
        if jobs.is_empty() {
            break;
        }
        let t = execute_scan(&daemon, jobs, opts).await;
        if t.held {
            // No-degrade: the engine/summariser is down. Report it loudly and
            // stop — cursors stayed put, so the next trigger resumes exactly here.
            daemon.with_status(|s| {
                s.set_phase(
                    Phase::Offline,
                    "Summariser/engine unavailable — work held, retrying",
                );
                s.set_stat("segments_sending", json!(0));
            });
            return;
        }
        if !t.more_pending {
            break;
        }
        // Backlog still draining — yield briefly so the receiver/heartbeat aren't
        // starved, then take the next newest cap-12 batch.
        daemon.with_status(|s| s.set_phase(Phase::Processing, "Catching up on history…"));
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    daemon.with_status(|s| {
        s.set_phase(Phase::Watching, "Waiting for new events");
        s.set_progress(0, 0);
    });
}

/// Force-scan a specific session/file (the loopback control endpoint's eager
/// scan), then refresh that session's server insights into the local cache the
/// statusline reads. A warm daemon already has the summariser resident, so a
/// just-finished session lands in seconds. Runs safely alongside the periodic
/// file scan — both take the state lock, so they serialise. Port of
/// `runEagerSessionScan` + `scanSession`.
pub async fn scan_session(daemon: Arc<Daemon>, session_ids: Vec<String>, file: Option<String>) {
    daemon.with_status(|s| s.set_phase(Phase::Scanning, "Eager scan (current session)"));
    let jobs = eager_target_jobs(&session_ids, file.as_deref());
    if jobs.is_empty() {
        daemon.with_status(|s| {
            s.set_phase(Phase::Watching, "Waiting for new events");
            s.set_message("Eager scan: no matching transcript");
        });
        return;
    }
    let opts = RunScanOptions {
        max_files: None,
        force_read_all: true,
    };
    let t = execute_scan(&daemon, jobs, opts).await;

    // Refresh the local insights cache for the scanned session chain so the
    // statusline shows fresh numbers. Only when explicit ids were given (the
    // server resolves nothing useful from a bare file).
    if !session_ids.is_empty() {
        let fetcher = McpInsightsFetcher {
            api: (*daemon.api).clone(),
        };
        refresh_session_insights(&fetcher, &session_ids, &home_path("sessions"), now_iso).await;
    }

    daemon.with_status(|s| {
        s.set_phase(Phase::Watching, "Waiting for new events");
        s.set_message(if t.segments_uploaded > 0 {
            format!("Eager scan: {} segments uploaded", t.segments_uploaded)
        } else {
            "Eager scan: nothing new".to_string()
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(cwd: Option<&str>) -> RawEvent {
        RawEvent {
            source_event_id: "e1".into(),
            ts: "2026-07-16T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s1".into(),
            turn_index: None,
            parent_event_id: None,
            cwd: cwd.map(Into::into),
            git: None,
            tokens: None,
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            pricing_mode: None,
        }
    }

    #[test]
    fn correct_events_leaves_cwd_less_events_untouched() {
        // No cwd on any event → resolve_authoritative_git returns them unchanged
        // WITHOUT invoking git (deterministic; the real resolver is never called).
        let mut correct = make_correct_events();
        let events = vec![ev(None), ev(None)];
        let out = correct(events.clone());
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|e| e.git.is_none()));
    }

    // make_extract_links is a thin concrete wrapper over the (tested) link_extract
    // pass; this test just pins that it constructs + type-checks as a Send boxed
    // future (a generic engine would fail to compile here — the whole point).
    #[test]
    fn extract_links_constructs_over_a_concrete_client() {
        let client = SummarizerClient::new("http://127.0.0.1:0");
        let _extractor: Box<LinkExtractor<'_>> = make_extract_links(&client);
    }

    #[test]
    fn now_iso_is_utc_millis_with_z() {
        let s = now_iso();
        // 2026-07-16T10:00:00.000Z shape: has the T, the millis dot, trailing Z.
        assert!(s.ends_with('Z'), "{s}");
        assert!(s.contains('T'), "{s}");
        assert_eq!(&s[19..20], ".", "expected millis separator: {s}");
        assert_eq!(s.len(), 24, "ISO millis + Z is 24 chars: {s}");
    }

    #[test]
    fn kind_for_path_reads_the_directory_shape() {
        assert_eq!(
            kind_for_path("/h/.codex/sessions/2026/07/16/rollout-x.jsonl"),
            ParserKind::Codex
        );
        assert_eq!(kind_for_path("/h/.pi/agent/sessions/p/x.jsonl"), ParserKind::Pi);
        assert_eq!(
            kind_for_path("/h/.claude/projects/p/x.jsonl"),
            ParserKind::ClaudeCode
        );
    }

    #[test]
    fn session_id_for_path_maps_a_claude_uuid_filename() {
        let sid =
            session_id_for_path("/h/.claude/projects/p/33333333-3333-3333-3333-333333333333.jsonl");
        assert_eq!(sid.as_deref(), Some("33333333-3333-3333-3333-333333333333"));
        // A non-transcript name maps to nothing (never a false id).
        assert!(session_id_for_path("/h/.claude/projects/p/notes.txt").is_none());
    }

    #[test]
    fn read_capped_truncates_to_the_byte_cap() {
        let dir = std::env::temp_dir().join(format!("modelstat-rc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.sh");
        std::fs::write(&path, vec![b'x'; MAX_SCRIPT_READ_BYTES + 500]).unwrap();
        let got = read_capped(path.to_str().unwrap()).unwrap();
        assert_eq!(got.len(), MAX_SCRIPT_READ_BYTES);
        assert!(read_capped("/no/such/file").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_observer_drives_phase_progress_and_stats() {
        let status = StdMutex::new(Status::default());
        let mut obs = StatusObserver { status: &status };
        obs.on_file("/a/b/c.jsonl", 0, 3);
        obs.on_upload(10, 4);
        obs.on_uploaded(9, 4);
        let s = status.lock().unwrap();
        assert_eq!(s.progress_done, 1);
        assert_eq!(s.progress_total, 3);
        assert_eq!(s.phase, Phase::Uploading);
        assert_eq!(s.stats["events_uploaded"], json!(9));
        assert_eq!(s.stats["batches_uploaded"], json!(1));
        assert_eq!(s.stats["segments_sending"], json!(0)); // reset after upload
        assert!(s.last_event_at.is_some());
    }

    #[test]
    fn eager_target_jobs_builds_an_adhoc_job_for_an_unknown_file() {
        // An explicit file discovery hasn't enumerated still yields a job with the
        // dir-shape parser (so a brand-new file mid-write is scannable).
        let jobs = eager_target_jobs(&[], Some("/somewhere/.codex/sessions/2026/07/16/rollout-z.jsonl"));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, ParserKind::Codex);
        assert!(jobs[0].path.ends_with("rollout-z.jsonl"));
    }
}
