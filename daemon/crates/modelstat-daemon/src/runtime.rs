//! Daemon-main runtime wiring — the closures + scan wrappers + the top-level
//! `run` loop that compose every M4 primitive into the live collector process.
//!
//! This file grows the daemon-main composition. It starts
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

use crate::anchors::AnchorMiner;
use crate::authoritative_git::resolve_authoritative_git;
use crate::discover_jobs::{
    discover_jobs, order_jobs_newest_first, parse_job_streaming, ParserKind, ScanJob,
};
use crate::engine::{
    build_embedder, build_redactor, engine_base_url, DaemonEmbedder, DaemonRedactor, RemoteConfig,
    Swappable,
};
use crate::insights::{refresh_session_insights, SessionInsightsFetcher};
use crate::scan::{run_scan_over_jobs, CursorStore, RunScanOptions, ScanObserver, ScanTallies};
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
    Box::new(
        move |abstracts: Vec<String>| -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
            Box::pin(async move { link_extract(engine, &abstracts).await })
        },
    )
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

/// How much redacted-but-unsent data the spool holds before the scan pauses
/// (`MODELSTAT_SPOOL_MAX_BYTES`, else [`crate::spool::DEFAULT_MAX_SPOOL_BYTES`]).
/// An env knob because the right answer depends on the disk: a build box with
/// 40 GB free can ride out an outage a laptop cannot.
fn max_spool_bytes() -> u64 {
    std::env::var("MODELSTAT_SPOOL_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(crate::spool::DEFAULT_MAX_SPOOL_BYTES)
}

/// The shared, process-lifetime handles every daemon subsystem reads. All heavy
/// state is already behind `Arc`/`Mutex`, so the scan runner, heartbeat, SDK
/// drain, reconcile, and watcher each hold a cheap `Arc<Daemon>` clone.
pub struct Daemon {
    pub config: Arc<Config>,
    pub api: Arc<DeviceApi>,
    pub resilient: Arc<ResilientSummarizer<SummarizerClient>>,
    /// Swappable: the boot-time self-heal replaces these in place when a missing
    /// model finishes downloading, so a blip during `connect` costs minutes of
    /// degraded quality instead of lasting until the next restart.
    pub embedder: Arc<Swappable<DaemonEmbedder>>,
    pub redactor: Arc<Swappable<DaemonRedactor>>,
    /// Server-delivered config (`policies`, `calibration`). Seeded from disk at
    /// build so a scan is never gated on the network, refreshed on a timer by
    /// [`crate::run`]. Adopting a payload installs it where it is enforced, so
    /// nothing on the scan path reads this handle.
    pub remote_config: Arc<RemoteConfig>,
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
    /// Redacted batches waiting for the wire. The scan pushes, the upload loop
    /// drains. This is what makes an outage cost a retry instead of a re-run of
    /// the PII model over every turn again.
    pub spool: Arc<crate::spool::Spool>,
    /// The pre-AI baseline miner, or None when this device opted out
    /// (`MODELSTAT_ANCHORS=0`). Process-lifetime because "mine each repo once
    /// per run" is state only the run can hold.
    pub anchors: Option<AnchorMiner>,
    pub device_id: String,
    pub machine_id: String,
    /// The install-time summariser mode, resolved ONCE (a change bounces the
    /// service): `local` / `self-hosted` / `cloud`. Boot-constant BY CONTRACT —
    /// it decides batch shape and engine wiring, which are built here.
    pub mode: String,
    /// The redactor mode the CURRENT `redactor` handle was built for. The scan
    /// compares it to the live setting each cycle and rebuilds on drift, so a
    /// mode switch takes effect without depending on the CLI's service bounce.
    pub redactor_mode_built: StdMutex<String>,
    /// Auto-update dedup (§13): the `(verdict, target)` keys already acted on this
    /// process, so a heartbeat every 10s never stacks a second self-update.
    /// When each (verdict, target) was last ATTEMPTED — an expiring dedup, so a
    /// failed update is retried rather than skipped forever. See
    /// `modelstat_update::AUTO_UPDATE_RETRY_AFTER_MS`.
    pub handled_updates: Arc<StdMutex<std::collections::HashMap<String, i64>>>,
}

impl Daemon {
    /// Construct the shared handles from config: load persisted state, build the
    /// engine client for the active mode, and load the on-device models (real
    /// candle or fail-safe). Starts NO task — [`run`] owns the loop.
    pub fn build(
        config: Arc<Config>,
        device_id: String,
        machine_id: String,
    ) -> std::io::Result<Arc<Self>> {
        let mode = config.summarizer_mode();
        let engine = SummarizerClient::new(engine_base_url(&config));
        // Fallible, and deliberately so: without a spool the daemon has nowhere
        // durable to put a redacted batch, and carrying on would mean either
        // dropping data or re-redacting it forever. Better to refuse to start and
        // say why.
        let spool = crate::spool::Spool::open(home_path("spool"), max_spool_bytes())?;
        Ok(Arc::new(Daemon {
            api: Arc::new(DeviceApi::new(config.clone())),
            resilient: Arc::new(ResilientSummarizer::new(engine)),
            embedder: Arc::new(Swappable::new(build_embedder())),
            redactor: Arc::new(Swappable::new(build_redactor(&config))),
            // Before the first scan, always: whatever config this device last
            // saw is in force from the very first turn it redacts.
            remote_config: Arc::new(RemoteConfig::load()),
            state: Arc::new(TokioMutex::new(load_state())),
            status: Arc::new(StdMutex::new(Status::default())),
            queue: Arc::new(FileQueueStore::new(home_path("queue.json"))),
            spool: Arc::new(spool),
            anchors: AnchorMiner::from_env(home_path("anchors.json")),
            device_id,
            machine_id,
            mode,
            redactor_mode_built: StdMutex::new(config.redactor_mode()),
            config,
            handled_updates: Arc::new(StdMutex::new(std::collections::HashMap::new())),
        }))
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
    fn on_session_activity(
        &mut self,
        session_id: &str,
        agent: &str,
        label: Option<&str>,
        last_ms: i64,
    ) {
        self.with(|s| s.note_live(session_id, agent, label, last_ms));
    }

    fn on_file(&mut self, path: &str, index: usize, total: usize) {
        let _ = path;
        // A file's NAME is noise: it is a uuid the reader cannot act on, and it
        // made the line too long to read. How much is LEFT is the number that
        // answers "how long until this is done".
        self.with(|s| {
            s.set_progress(index as u64 + 1, total as u64);
            // The PHASE too, not just the message: the previous file left it on
            // `Uploading`, and a bare message change renders as the contradictory
            // "uploading — 12 sessions left".
            let line = progress_message(s);
            s.set_phase(Phase::Scanning, line);
            // Restarts the elapsed clock the tray ticks each second, so a file
            // that takes a while visibly shows work happening rather than a
            // frozen line.
            s.set_busy_now();
        });
    }
    fn on_spooled(&mut self, events: usize, segments: usize) {
        self.with(|s| {
            // "Processed", not "sent". These two words used to be the same number
            // because the scan did both; now a batch can be redacted and queued
            // while the network is down, and saying "sent" then would be a lie the
            // tray tells for as long as the outage lasts.
            s.bump_stat("events_processed", events as u64);
            s.bump_stat("batches_processed", 1);
            // Scoped to THIS sweep — the lifetime counters above are in the tens
            // of thousands after a few days and say nothing about the pass a
            // watcher is currently looking at. Files land once the pass ends, so
            // the two callers stay disjoint.
            s.bump_run(0, 0, events as u64, segments as u64);
            let line = progress_message(s);
            s.set_phase(Phase::Processing, line);
        });
    }
}

/// Feeds the spool drain into the same live [`Status`]. Separate from
/// [`StatusObserver`] on purpose: this is the only thing allowed to touch the
/// `*_uploaded` counters, so "sent" can only ever mean the server said yes.
///
/// Holds the two handles it needs rather than the whole `Daemon`, so the
/// reporting rules above can be unit-tested without booting a collector.
pub struct UploadStatusObserver {
    status: Arc<StdMutex<Status>>,
    state: Arc<TokioMutex<RuntimeState>>,
}

impl UploadStatusObserver {
    pub fn new(status: Arc<StdMutex<Status>>, state: Arc<TokioMutex<RuntimeState>>) -> Self {
        Self { status, state }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Status) -> R) -> R {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s)
    }
}

impl crate::uploader::UploadObserver for UploadStatusObserver {
    fn on_pass_start(&mut self, batches: u64) {
        self.with(|s| {
            s.start_upload_set(batches, batches);
            s.set_queue(batches);
            let line = progress_message(s);
            s.set_phase(Phase::Uploading, line);
        });
    }

    fn on_uploaded(&mut self, events: u64, segments: usize) {
        let iso = now_iso();
        self.with(|s| {
            s.bump_stat("events_uploaded", events);
            s.bump_stat("batches_uploaded", 1);
            s.set_stat("segments_sending", json!(segments));
            s.finish_one_upload();
            s.note_event_at(iso);
            // Refresh the line so the "N events sent" counter visibly climbs while
            // a backlog drains, instead of freezing until the pass ends.
            if s.phase == Phase::Uploading {
                let line = progress_message(s);
                s.set_message(line);
            }
        });
    }

    fn on_pass_end(&mut self, outcome: &crate::uploader::DrainOutcome) {
        let depth = outcome.depth;
        let rejected = outcome.rejected;
        self.with(|s| {
            s.set_queue(depth.batches);
            s.set_stat("segments_sending", json!(0));
            // The count of batches the server refuses on their CONTENT, published
            // where a human already looks: `modelstat status`, the tray, and the
            // heartbeat the dashboard shows. The incident that added this line was
            // invisible for 14 hours precisely because every surface kept saying
            // "scanning" while nothing had shipped since the first refusal.
            s.set_stat("batches_refused", json!(rejected));
            // Nothing left on the wire. Said explicitly, or a reader keeps
            // claiming "3 sessions uploading" through the whole backoff.
            s.start_upload_set(0, 0);
            // Only stand down from a phase WE set. The drain and a scan run at the
            // same time now, and a finished upload must not overwrite "scanning —
            // 12 session files left" with "idle" while the scan is still going.
            if s.phase == Phase::Uploading {
                let line = progress_message(s);
                if rejected > 0 {
                    // Never "idle" while the server is refusing work: that is the
                    // lie that hid this failure.
                    s.set_phase(
                        Phase::Processing,
                        format!("{line} — {rejected} batch(es) the server refused"),
                    );
                } else if depth.batches == 0 {
                    s.set_phase(Phase::Idle, line);
                } else {
                    s.set_phase(Phase::Processing, line);
                }
            }
        });
        // The lifetime segments-sent total is persisted, so it moves HERE — where
        // segments genuinely reach the server — rather than when a scan finishes.
        // Spawned because this hook is sync and the state lock is async; the
        // counter is a display total, so a few ms of lag costs nothing.
        if outcome.segments_uploaded > 0 {
            let segments = outcome.segments_uploaded;
            let state = self.state.clone();
            let status = self.status.clone();
            tokio::spawn(async move {
                let total = {
                    let mut guard = state.lock().await;
                    guard.segments_sent += segments as i64;
                    let total = guard.segments_sent;
                    if let Err(e) = save_state(&guard) {
                        modelstat_log::log_warn!("could not persist segments_sent: {e}");
                    }
                    total
                };
                let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                s.set_stat("segments_sent", json!(total));
            });
        }
    }
}

/// The ONE progress sentence every phase carries: how many session files are
/// still to get through, counting the one being read right now.
///
/// One number, one meaning, whichever loop wrote it. The scan said "N session
/// files left" while the uploader said "file 3/71" and the tray said "168 new,
/// 2089 skipped" — three renderings of one fact, which a reader has to
/// reconcile before learning anything. The uploader's fraction was the worst of
/// them: the same number as the scan's with the subtraction left undone.
///
/// No event or segment figure here. That total lives in exactly one place
/// (`stats.events_uploaded`); a message that carried it too rendered the same
/// count three times over with one word of difference.
fn progress_message(s: &Status) -> String {
    if s.progress_total == 0 {
        return "shipping batches".to_string();
    }
    // `progress_done` counts files VISITED, the one in progress included — so the
    // file on the wire is still work outstanding, and the count only reaches
    // "last" on the final file.
    match s.progress_total.saturating_sub(s.progress_done) + 1 {
        0 | 1 => "last session file".to_string(),
        n => format!("{} session files left", thousands(n)),
    }
}

/// `12345` → `"12,345"`. The tray menu and `modelstat status` are read at a
/// glance; a bare five-digit count is not.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
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
    } else if path.contains("/.pi/agent/sessions/") || path.contains("/.omp/agent/sessions/") {
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
    if path.contains("/.pi/agent/sessions/") || path.contains("/.omp/agent/sessions/") {
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
            None => {
                let mut adhoc = vec![ScanJob {
                    agent_label: None,
                    since_ms: None,
                    path: f.to_string(),
                    kind: kind_for_path(f),
                }];
                // Discovered jobs carry their harness label already; an
                // ad-hoc target earns it the same way.
                crate::discover_jobs::apply_harness_labels(&mut adhoc);
                adhoc
            }
        };
    }
    if !session_ids.is_empty() {
        let wanted: std::collections::BTreeSet<&str> =
            session_ids.iter().map(String::as_str).collect();
        return all
            .into_iter()
            .filter(|j| {
                session_id_for_path(&j.path).is_some_and(|sid| wanted.contains(sid.as_str()))
            })
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
    // Every redactor mode scans. `cloud`/`self-hosted` classify spans remotely
    // over floor-scrubbed text, and their failure shape is the same fail-closed
    // hold the local model has: `pii_redact_checked` answers `None`, the flush
    // holds, nothing unscrubbed ever leaves (§9.5). A misconfigured mode (no
    // URL, unpaired device) builds as `UnavailableRedactor`, which holds the same
    // way — loudly, at build time, with the fix in the log.
    let device_id = daemon.device_id.as_str();
    let daemon_version = daemon.config.version();

    // The link-extraction seam borrows the concrete engine so its boxed future is
    // provably `Send` (a generic engine can't satisfy that under async-fn-in-trait).
    let extractor = make_extract_links(daemon.resilient.engine());
    let mut correct = make_correct_events();
    let mut git = RealGitEnrichment::new();
    // Where finished batches go. Not the network — the upload loop owns that, and
    // keeping the two apart is what stops an outage from re-running the redactor.
    // Stamped at the door with the redactor mode THIS scan classified under, so
    // a mode switch while batches sit spooled cannot mislabel them.
    let sink = crate::adapters::StampedSink {
        spool: &daemon.spool,
        redactor_mode: daemon.config.redactor_mode(),
        // `daemon.mode`, not a fresh config read: the batch SHAPE (raw vs
        // segments) was decided by the mode this process booted with, and the
        // stamp must name the mode that actually built it.
        summarizer_mode: daemon.mode.clone(),
        // Mined here rather than in the batch builder: anchors describe the
        // REPOS a batch touched, and this door is where every batch passes
        // regardless of which mode built it.
        anchors: daemon.anchors.as_ref(),
    };
    let sink = &sink;
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
            shipped_through_ms: None,
            size: c.size,
            mtime: c.mtime as i64,
            tail_hash: c.tail_hash,
        })
    };

    // A changed redactor setting takes effect at the next scan, not the next
    // service bounce: the handle rebuilds when the stored mode drifts from the
    // one it was built for. (The summariser mode stays boot-constant — it
    // decides batch shape and engine wiring — and `modelstat mode` bounces.)
    {
        let want = daemon.config.redactor_mode();
        let mut built = daemon
            .redactor_mode_built
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if *built != want {
            modelstat_log::log_info!(
                "redactor mode {} → {want} — rebuilding the redactor",
                *built
            );
            daemon.redactor.set(build_redactor(&daemon.config));
            *built = want;
        }
    }

    // Snapshot the model handles for this scan: a mid-scan self-heal swap must
    // not change which embedder half a session was built with.
    let embedder = daemon.embedder.get();
    let redactor = daemon.redactor.get();

    // Snapshot the logged-in accounts ONCE for the whole scan, for the same
    // reason: the heartbeat rewrites this file every 10s, and a switch landing
    // mid-scan must not split one pass across two accounts. Empty (no file yet,
    // or nothing detected) simply names nothing and the server infers.
    let accounts = modelstat_ingest::accounts::load_accounts();

    let mut guard = daemon.state.lock().await;
    let tallies = run_scan_over_jobs(
        ordered,
        device_id,
        daemon_version,
        &daemon.mode,
        opts,
        &daemon.resilient,
        &*embedder,
        &*redactor,
        &mut git,
        Some(&*extractor),
        parse,
        checksum,
        &mut correct,
        &exists,
        &read_file,
        sink,
        &mut *guard as &mut (dyn CursorStore + Send),
        &mut observer,
        &accounts,
    )
    .await;

    // Persist the advanced cursors. The segments-sent lifetime total is NOT folded
    // here any more — a spooled segment has not been sent, and the upload loop
    // bumps that counter when the server actually takes it.
    let segments_sent = guard.segments_sent;
    let _ = save_state(&guard);
    drop(guard);

    // Anything this scan produced is on disk and waiting; poke the uploader so it
    // goes out now rather than on its next backstop tick. Without this the spool
    // would add up to a minute of latency to every batch, which is exactly the
    // trade the spool is not allowed to make. (Each `push` pokes it too — this is
    // the end-of-scan backstop for a pass that was already mid-flight.)
    daemon.spool.wake().notify_one();

    daemon.with_status(|s| {
        s.bump_stat("files_scanned", tallies.files_scanned as u64);
        s.bump_stat("files_unchanged", tallies.files_unchanged as u64);
        s.bump_stat("files_silent", tallies.files_silent as u64);
        s.bump_stat("files_failed", tallies.files_failed as u64);
        s.bump_stat(
            "records_skipped",
            tallies.skipped_kinds.values().sum::<u64>(),
        );
        s.bump_skipped_kinds(tallies.skipped_kinds.clone());
        // Files only: this sweep's events + segments were already folded in per
        // batch by `on_uploaded`, and counting them twice would inflate the row.
        s.bump_run(
            tallies.files_scanned as u64,
            tallies.files_unchanged as u64,
            0,
            0,
        );
        s.set_stat("segments_sent", json!(segments_sent));
        s.set_stat("segments_sending", json!(0));
    });
    tallies
}

/// Incremental scan, newest-first. Unchanged files are skipped and are not
/// counted as leftover work. A hold (engine/summariser down) stops the run
/// loudly — the next trigger (watcher / 5-min backstop) retries from exactly
/// where it held.
pub async fn run_scan_cycle(daemon: Arc<Daemon>, reason: String) {
    daemon.with_status(|s| {
        s.set_phase(Phase::Scanning, format!("Scanning local JSONL ({reason})"));
        s.set_progress(0, 0);
        s.start_run();
    });
    let opts = RunScanOptions {
        max_files: None,
        force_read_all: false,
    };
    let jobs = order_jobs_newest_first(discover_jobs());
    if !jobs.is_empty() {
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
                s.clear_busy();
            });
            return;
        }
    }
    // The backlog drained — the ONE moment this daemon knows nothing is queued,
    // and therefore the only honest place to declare a version bump's re-scan
    // finished. A held run returned above with the mandate still outstanding.
    settle_rescans(&daemon).await;
    daemon.with_status(|s| {
        s.set_phase(Phase::Watching, "Waiting for new events");
        s.set_progress(0, 0);
        s.clear_busy();
    });
}

/// Advance any aspect whose mandated re-scan has actually finished, and refresh
/// the line the status surfaces show for the ones that have not.
///
/// The completion test is "no discovered file this aspect owns is still missing
/// a cursor". Discovery is re-run here rather than reused from the sweep so the
/// answer reflects the tree as it is NOW: files that appeared mid-sweep are
/// still owed, and files that VANISHED mid-sweep are owed nothing — a deleted
/// transcript cannot be re-read, and treating it as outstanding work would
/// re-wipe the corpus on every boot forever.
async fn settle_rescans(daemon: &Arc<Daemon>) {
    let discovered: std::collections::BTreeMap<String, &'static str> = discover_jobs()
        .into_iter()
        .map(|j| (j.path, j.kind.aspect()))
        .collect();
    let (notes, pending) = {
        let mut state = daemon.state.lock().await;
        let done = crate::processing_version::settle_processing_rescans(&mut *state, &discovered);
        if done.changed {
            if let Err(e) = save_state(&state) {
                modelstat_log::log_warn!("couldn't persist the completed re-scan: {e}");
            }
        }
        (
            done.notes,
            crate::processing_version::rescans_in_progress(&*state, &discovered),
        )
    };
    for note in &notes {
        modelstat_log::log_info!("pipeline reconcile: {note}");
    }
    // Still owed after a drained sweep means files the scan could not read
    // (a parse error holds its cursor back). Say so every sweep — silence here
    // is what let a stalled repair look like a healthy daemon.
    for p in &pending {
        modelstat_log::log_info!("pipeline reconcile: {p}");
    }
    daemon.with_status(|s| s.set_rescan(crate::processing_version::rescan_line(&pending)));
}

/// Force-scan a specific session/file (the loopback control endpoint's eager
/// scan), then refresh that session's server insights into the local cache the
/// statusline reads. A warm daemon already has the summariser resident, so a
/// just-finished session lands in seconds. Runs safely alongside the periodic
/// file scan — both take the state lock, so they serialise. Port of
/// `runEagerSessionScan` + `scanSession`.
pub async fn scan_session(daemon: Arc<Daemon>, session_ids: Vec<String>, file: Option<String>) {
    daemon.with_status(|s| {
        s.set_phase(Phase::Scanning, "Eager scan (current session)");
        s.start_run();
    });
    let jobs = eager_target_jobs(&session_ids, file.as_deref());
    if jobs.is_empty() {
        daemon.with_status(|s| {
            s.set_phase(Phase::Watching, "Waiting for new events");
            s.set_message("Eager scan: no matching transcript");
            s.clear_busy();
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
        // Whether it found work, not how much: a segment count here is a fifth
        // kind of number on a surface that shows four, and the one total anyone
        // reads (`stats.events_uploaded`) already moves when this lands.
        s.set_message(if t.segments_spooled > 0 {
            "Eager scan: queued to send"
        } else {
            "Eager scan: nothing new"
        });
        // The periodic sweep has always ended this way; the eager one never did,
        // so it left `busy_since_ms` pointing at its last file and readers went on
        // ticking an elapsed clock against a daemon that had gone back to idle.
        s.clear_busy();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(cwd: Option<&str>) -> RawEvent {
        RawEvent {
            seq: None,
            started_at: None,
            first_token_at: None,
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            source_event_id: "e1".into(),
            ts: "2026-07-16T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s1".into(),
            actor_id: None,
            recipient_actor_id: None,
            turn_index: None,
            parent_event_id: None,
            cwd: cwd.map(Into::into),
            git: None,
            tokens: None,
            tokens_unmapped: std::collections::BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            tool_paths: Vec::new(),
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
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
        assert_eq!(
            kind_for_path("/h/.pi/agent/sessions/p/x.jsonl"),
            ParserKind::Pi
        );
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

    fn upload_observer() -> (Arc<StdMutex<Status>>, UploadStatusObserver) {
        let status = Arc::new(StdMutex::new(Status::default()));
        let obs = UploadStatusObserver::new(
            status.clone(),
            Arc::new(TokioMutex::new(RuntimeState::default())),
        );
        (status, obs)
    }

    fn drained(
        batches: u64,
        segments: u64,
        held: bool,
        left: u64,
    ) -> crate::uploader::DrainOutcome {
        crate::uploader::DrainOutcome {
            batches_uploaded: batches,
            events_uploaded: 0,
            segments_uploaded: segments,
            held,
            rejected: 0,
            depth: crate::spool::SpoolDepth {
                batches: left,
                bytes: left * 100,
            },
        }
    }

    #[test]
    fn the_upload_observer_reports_what_is_on_the_wire_and_clears_it_after() {
        use crate::uploader::UploadObserver;
        let (status, mut obs) = upload_observer();
        // Three batches go out together — the count a watcher sees, plus a clock.
        obs.on_pass_start(3);
        {
            let s = status.lock().unwrap();
            let u = s.uploading.clone().expect("in flight");
            assert_eq!((u.uploads, u.sessions), (3, 3));
            assert_eq!(s.queue_size, 3);
        }
        obs.on_uploaded(10, 0);
        assert_eq!(
            status.lock().unwrap().uploading.as_ref().unwrap().uploads,
            2
        );
        obs.on_uploaded(10, 0);
        obs.on_uploaded(10, 0);
        assert!(
            status.lock().unwrap().uploading.is_none(),
            "the wire is quiet once the last one commits"
        );

        // A held pass: one commits, the rest stay spooled. The tray must say the
        // wire is quiet, and say how many are still waiting.
        obs.on_pass_start(3);
        obs.on_uploaded(10, 0);
        obs.on_pass_end(&drained(1, 0, true, 2));
        let s = status.lock().unwrap();
        assert!(
            s.uploading.is_none(),
            "a hold must not leave the tray claiming an upload through the backoff"
        );
        assert_eq!(s.queue_size, 2, "and it must say what is still waiting");
    }

    /// The honesty split this change exists to protect: the SCAN counts what it
    /// processed, and only the UPLOADER may count what was sent. During an outage
    /// the first climbs and the second must not move at all.
    ///
    /// Async because a completed pass persists the lifetime segments-sent total on
    /// the runtime, exactly as it does in the daemon.
    #[tokio::test]
    async fn processed_and_sent_are_separate_counters() {
        use crate::uploader::UploadObserver;
        let status = Arc::new(StdMutex::new(Status::default()));
        {
            let mut scan_obs = StatusObserver { status: &status };
            scan_obs.on_file("/a/b/c.jsonl", 0, 3);
            scan_obs.on_spooled(10, 4);
        }
        {
            let s = status.lock().unwrap();
            assert_eq!(s.progress_done, 1);
            assert_eq!(s.progress_total, 3);
            assert_eq!(s.stats["events_processed"], json!(10));
            assert_eq!(s.stats["batches_processed"], json!(1));
            assert!(
                !s.stats.contains_key("events_uploaded"),
                "nothing has been SENT yet — the scan must not claim otherwise"
            );
        }

        // Now the uploader lands it, and only now does "sent" move.
        let mut up_obs = UploadStatusObserver::new(
            status.clone(),
            Arc::new(TokioMutex::new(RuntimeState::default())),
        );
        up_obs.on_pass_start(1);
        up_obs.on_uploaded(9, 4);
        up_obs.on_pass_end(&drained(1, 4, false, 0));
        let s = status.lock().unwrap();
        assert_eq!(s.stats["events_uploaded"], json!(9));
        assert_eq!(s.stats["batches_uploaded"], json!(1));
        assert_eq!(s.stats["segments_sending"], json!(0)); // reset after the pass
        assert_eq!(s.queue_size, 0);
        assert!(s.last_event_at.is_some());
    }

    /// A finished upload must not overwrite a running scan's phase — the two loops
    /// are independent now, and the scan's line is the more informative one.
    #[test]
    fn a_finished_upload_does_not_stomp_on_a_running_scan() {
        use crate::uploader::UploadObserver;
        let (status, mut obs) = upload_observer();
        status
            .lock()
            .unwrap()
            .set_phase(Phase::Scanning, "3 session files left");
        obs.on_pass_end(&drained(1, 0, false, 0));
        let s = status.lock().unwrap();
        assert_eq!(s.phase, Phase::Scanning);
        assert_eq!(s.message.as_deref(), Some("3 session files left"));
    }

    #[test]
    fn each_phase_reports_what_is_actually_happening() {
        // The phase must track the work, not stick on the last thing that moved:
        // a message-only update rendered as "uploading — File 1/3: …", and a
        // committed batch left the tray claiming an upload through the (long)
        // parse + summarise of the next one.
        let status = StdMutex::new(Status::default());
        let mut obs = StatusObserver { status: &status };

        obs.on_file("/a/b/c.jsonl", 0, 3);
        assert_eq!(status.lock().unwrap().phase, Phase::Scanning);
        // What is LEFT, and no file name: the name is a uuid the reader cannot
        // act on, and it pushed the line past the width of the menu.
        assert_eq!(
            status.lock().unwrap().message.as_deref(),
            Some("3 session files left")
        );
        // A clock the reader can watch tick — the difference between "working"
        // and "wedged" on a line that happens not to change.
        assert!(
            status.lock().unwrap().busy_since_ms.is_some(),
            "starting a file starts the elapsed clock"
        );

        // Parking a finished batch is PROCESSING, not uploading — nothing has
        // left the machine yet.
        obs.on_spooled(10, 4);
        assert_eq!(status.lock().unwrap().phase, Phase::Processing);
    }

    #[test]
    fn the_remaining_count_counts_down_and_names_the_last_one() {
        let status = StdMutex::new(Status::default());
        let mut obs = StatusObserver { status: &status };
        let msg = || status.lock().unwrap().message.clone().unwrap_or_default();

        obs.on_file("/x/1.jsonl", 0, 652);
        assert_eq!(msg(), "652 session files left");
        obs.on_file("/x/2.jsonl", 82, 652);
        assert_eq!(msg(), "570 session files left", "counts down, never up");
        obs.on_file("/x/3.jsonl", 651, 652);
        assert_eq!(msg(), "last session file", "no '1 session files left'");

        // A backlog is four and five digits deep, and this is read at a glance.
        obs.on_file("/x/4.jsonl", 0, 12_400);
        assert_eq!(msg(), "12,400 session files left");
    }

    #[test]
    fn the_uploader_repeats_the_scans_number_instead_of_inventing_a_second_one() {
        // Both loops write the same field, so both must render the same fact. The
        // uploader used to answer with a position fraction ("file 3/71") while the
        // scan answered with what is left ("69 session files left") — one quantity,
        // two spellings, and the reader does the subtraction to find that out.
        use crate::uploader::UploadObserver;
        let status = Arc::new(StdMutex::new(Status::default()));
        {
            let mut scan_obs = StatusObserver { status: &status };
            scan_obs.on_file("/a/b/c.jsonl", 2, 71);
        }
        let scan_line = status.lock().unwrap().message.clone();
        assert_eq!(scan_line.as_deref(), Some("69 session files left"));

        let mut obs = UploadStatusObserver::new(
            status.clone(),
            Arc::new(TokioMutex::new(RuntimeState::default())),
        );
        obs.on_pass_start(3);
        obs.on_uploaded(1000, 0);
        {
            let s = status.lock().unwrap();
            assert_eq!(
                s.message, scan_line,
                "an upload mid-sweep says what the sweep says"
            );
            assert!(
                s.uploading.is_some(),
                "the clock the reader watches is the upload set's, not the message's"
            );
        }
        // The elapsed clock on `uploading.since_ms` is what proves a long upload is
        // moving; the message is not asked to animate, so a second landed batch
        // leaves it alone rather than churning a number nobody is reading.
        obs.on_uploaded(1000, 0);
        assert_eq!(status.lock().unwrap().message, scan_line);
    }

    #[test]
    fn the_message_owns_work_left_and_the_stats_own_the_count() {
        // The message used to carry the events figure too — and after any
        // restart the pass counter, the lifetime counter and this message all
        // rendered the same number under one word ("events"), which read as
        // duplicate rows in the tray. One owner per fact now: the message says
        // how much work is LEFT, `stats.events_uploaded` says how much shipped.
        use crate::uploader::UploadObserver;
        let (status, mut obs) = upload_observer();

        obs.on_pass_start(2);
        obs.on_uploaded(120, 3); // segment batch (local / self-hosted)
        {
            let s = status.lock().unwrap();
            assert_eq!(s.message.as_deref(), Some("shipping batches"));
            assert_eq!(
                s.stats.get("events_uploaded").and_then(Value::as_u64),
                Some(120),
                "the count still exists — in exactly one place"
            );
        }

        obs.on_uploaded(0, 0); // raw event batch (cloud), 0 segments
        assert_eq!(
            status.lock().unwrap().message.as_deref(),
            Some("shipping batches"),
            "a zero-segment cloud batch cannot regress the line"
        );
    }

    #[test]
    fn thousands_groups_from_the_right() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(17_581), "17,581");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn eager_target_jobs_builds_an_adhoc_job_for_an_unknown_file() {
        // An explicit file discovery hasn't enumerated still yields a job with the
        // dir-shape parser (so a brand-new file mid-write is scannable).
        let jobs = eager_target_jobs(
            &[],
            Some("/somewhere/.codex/sessions/2026/07/16/rollout-z.jsonl"),
        );
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, ParserKind::Codex);
        assert!(jobs[0].path.ends_with("rollout-z.jsonl"));
    }
}
