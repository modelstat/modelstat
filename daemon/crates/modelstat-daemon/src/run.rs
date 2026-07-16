//! The daemon-main event loop — a port of `apps/daemon/src/daemon.ts`'s
//! `runDaemon`. Composes every M4 primitive into the live collector process:
//! singleton lock, heartbeat (discovery-fold), preflight, loopback receiver + SDK
//! drain, processing-version reconcile, the single-flight scan runner, the
//! filesystem watcher + 5-min backstop, the self-healing backfill reconcile, the
//! throttled last-status mirror, and a UNIFIED signal/shutdown path that quiesces
//! the in-flight scan before exit.
//!
//! Untestable-by-nature glue (it IS the process), so the load-bearing logic lives
//! in the unit-tested primitives this file wires (lock, scan, reconcile, status,
//! rotate, receiver); here we only sequence them per the boot map + own the
//! timers + the graceful teardown.

use std::collections::{BTreeMap, HashMap};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::oneshot;

use modelstat_ingest::state::save_state;
use modelstat_ingest::{home_path, Config};
use modelstat_parsers::discovery::{discover, DiscoveryOptions, DiscoveryOutput};
use modelstat_receiver::{
    drain_local_queue, start_local_ingest_receiver, ControlScanHandler, ControlTarget, QueueStore,
    DEFAULT_LOCAL_INGEST_PORT,
};

use crate::adapters::EnginePipeline;
use crate::discover_jobs::{discover_jobs, parse_job, ParserKind, ScanJob};
use crate::lock::{
    acquire_daemon_lock, check_lock_ownership, daemon_lock_path, format_age, is_process_alive,
    read_daemon_lock, remove_lock_if_owned, AcquireOpts, AcquireResult, OwnershipCheck,
    LOCK_RECHECK_MS,
};
use crate::processing_version::reconcile_processing_version;
use crate::reconcile::{reconcile_backfill, PerDaySession};
use crate::rotate::rotate_runaway_logs;
use crate::runtime::{now_iso, run_scan_cycle, scan_session, Daemon};
use crate::single_flight::CoalescingRunner;
use crate::status::{heartbeat_wire_body, write_last_status, Phase, UpdateInfo};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const SCAN_BACKSTOP_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DISCOVERY_BACKSTOP: Duration = Duration::from_secs(5 * 60);
const LOCAL_DRAIN_INTERVAL: Duration = Duration::from_secs(5);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30 * 60);
const RECONCILE_FIRST_DELAY: Duration = Duration::from_secs(60);
const WATCH_DEBOUNCE: Duration = Duration::from_secs(1);
const LOCAL_FLUSH_THROTTLE: Duration = Duration::from_millis(400);

/// The loopback ingest port (`MODELSTAT_LOCAL_INGEST_PORT`, else 4319 — must match
/// the SDKs' `DEFAULT_DAEMON_URL`).
fn ingest_port() -> u16 {
    std::env::var("MODELSTAT_LOCAL_INGEST_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_LOCAL_INGEST_PORT)
}

/// Run the collector daemon. Never returns until a signal (SIGINT → 130, SIGTERM
/// → 143) or a lost lock race (→ 0). Returns the process exit code. Port of
/// `runDaemon`.
pub async fn run(config: Arc<Config>, force: bool) -> ExitCode {
    // ── Enrollment guard ────────────────────────────────────────────────────
    let (Some(_bearer), Some(device_id)) = (config.bearer(), config.device_id()) else {
        eprintln!("modelstat: not enrolled — run `modelstat` (or `modelstat self-register`) first");
        return ExitCode::from(1);
    };

    // ── Singleton lock ──────────────────────────────────────────────────────
    let lock_path = daemon_lock_path();
    let acquire = acquire_daemon_lock(
        &lock_path,
        &AcquireOpts {
            daemon_version: config.version().to_string(),
            api_url: config.api_url(),
            force,
        },
    );
    match acquire {
        Ok(AcquireResult::AlreadyRunning { owner, age_sec }) => {
            println!(
                "modelstat daemon is already running — PID {}, started {} ago, daemon {}.",
                owner.pid,
                format_age(age_sec),
                owner.daemon_version
            );
            println!("  → to stop it:          kill {}", owner.pid);
            println!("  → to force-replace it: modelstat start --force");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("modelstat: could not acquire the daemon lock: {e}");
            return ExitCode::from(1);
        }
        Ok(AcquireResult::Acquired) => {}
    }

    // ── Boot ────────────────────────────────────────────────────────────────
    let machine_id = modelstat_ingest::intended_device_uuid();
    let daemon = Daemon::build(config.clone(), device_id.clone(), machine_id);
    daemon.with_status(|s| s.set_phase(Phase::Starting, "Booting"));
    // Trim runaway logs before anything else writes to them.
    rotate_runaway_logs();
    // Seed the lifetime segments-sent tally so the tray shows it from tick one.
    {
        let sent = daemon.state.lock().await.segments_sent;
        daemon.with_status(|s| s.set_stat("segments_sent", json!(sent)));
    }

    // The racing-daemon convergence recheck (feature §21.7): 5s after acquiring,
    // confirm we still own the lock — if a rival out-renamed us, stand down (0).
    let (lost_tx, lost_rx) = oneshot::channel::<()>();
    {
        let lock_path = lock_path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(LOCK_RECHECK_MS)).await;
            let current = read_daemon_lock(&lock_path);
            if check_lock_ownership(std::process::id() as i64, current.as_ref(), is_process_alive)
                == OwnershipCheck::Lost
            {
                let _ = lost_tx.send(());
            }
        });
    }

    // The single-flight scan runner — the load-bearing memory bound (never two
    // scans at once; triggers coalesce into one follow-up). `quiescing` latches
    // at shutdown so no new scan is admitted onto a device we're about to free.
    let quiescing = Arc::new(AtomicBool::new(false));
    let runner = {
        let d = daemon.clone();
        CoalescingRunner::new(move |reason: String| {
            let d = d.clone();
            async move { run_scan_cycle(d, reason).await }
        })
    };

    // ── Heartbeat ticker (+ prime) ──────────────────────────────────────────
    tokio::spawn(heartbeat_loop(daemon.clone()));

    // ── Preflight the summariser (never throws, never stops the daemon) ──────
    preflight(&daemon).await;

    // ── Loopback receiver + SDK drain ───────────────────────────────────────
    let control: ControlScanHandler = {
        let d = daemon.clone();
        Arc::new(move |t: ControlTarget| {
            let d = d.clone();
            Box::pin(async move {
                scan_session(d, t.session_ids.unwrap_or_default(), t.file).await;
            })
        })
    };
    let receiver =
        start_local_ingest_receiver(daemon.queue.clone(), ingest_port(), Some(control)).await;
    if receiver.is_some() {
        tokio::spawn(drain_loop(daemon.clone()));
    }

    // ── Reconcile the processing-pipeline version (wipe cursors on a bump) ────
    {
        let mut state = daemon.state.lock().await;
        let pv = reconcile_processing_version(&mut *state);
        if pv.changed {
            println!(
                "[modelstat] processing pipeline v{} → v{} — wiped file cursors so every session \
                 is re-processed by the new pipeline",
                pv.from, pv.to
            );
            let _ = save_state(&state);
        }
    }

    // ── First scan (awaited) ────────────────────────────────────────────────
    // Discovery rides the primed heartbeat, so go straight to scanning.
    runner.trigger("startup".to_string());
    runner.idle().await;

    // ── Filesystem watcher + 5-min backstop + reconcile timers ──────────────
    let _watcher = spawn_watcher(runner.clone(), quiescing.clone());
    tokio::spawn(backstop_loop(runner.clone(), quiescing.clone()));
    tokio::spawn(reconcile_loop(daemon.clone(), runner.clone(), quiescing.clone()));

    // ── Throttled last-status mirror (tray/statusline read this file) ───────
    tokio::spawn(last_status_loop(daemon.clone()));

    // ── Park until a signal or a lost lock race, then quiesce + exit ────────
    let code = wait_for_shutdown(lost_rx).await;
    quiescing.store(true, Ordering::SeqCst);
    daemon.with_status(|s| s.set_phase(Phase::Offline, "Shutting down"));
    // Final liveness so the dashboard flips to offline immediately (bare — no
    // discovery fold at teardown).
    post_heartbeat_now(&daemon, None).await;
    if let Some(r) = &receiver {
        r.abort();
    }
    // Let the in-flight scan drain before we exit (frees the engine cleanly).
    runner.idle().await;
    remove_lock_if_owned(&lock_path, std::process::id() as i64);
    ExitCode::from(code)
}

/// Boot preflight (feature §9.4): cloud → a plain server-side label; local /
/// self-hosted → healthz + smoke completion over the resilient engine's raw
/// client (fast-fail, so a down engine surfaces NOW as a loud held status rather
/// than hanging boot). Reports; never throws, never stops the daemon.
async fn preflight(daemon: &Daemon) {
    if daemon.mode == "cloud" {
        daemon.with_status(|s| {
            s.set_message("cloud — modelstat summarises server-side (no local model)")
        });
        return;
    }
    let label = if daemon.mode == "self-hosted" {
        format!("self-hosted summarizer at {}", daemon.config.self_hosted_url())
    } else {
        "local summarizer (Qwen3.5-4B)".to_string()
    };
    // Smoke the RAW engine (fast-fail): a down engine surfaces NOW as a loud held
    // status instead of hanging boot on the resilient wrapper's retries.
    let report = modelstat_pipeline::preflight(&label, daemon.resilient.engine()).await;
    if report.available {
        println!("[modelstat] summariser preflight ok: {}", report.message);
        daemon.with_status(|s| s.set_message(format!("summariser ready: {label}")));
    } else {
        eprintln!("[modelstat] ⚠ summariser unavailable — {}", report.message);
        daemon.with_status(|s| {
            s.set_message(format!(
                "summariser unavailable ({label}) — summaries held, retrying"
            ))
        });
    }
}

/// The 10s heartbeat loop (+ an immediate prime), owning the discovery-fold state:
/// attach the installs/identities snapshot on the FIRST beat, whenever it changes,
/// or on the 5-min backstop — else a bare liveness body. A `discover()` failure is
/// swallowed (liveness still ships). Port of `sendHeartbeat` +
/// `discoverySnapshotForHeartbeat`.
async fn heartbeat_loop(daemon: Arc<Daemon>) {
    let mut last_snapshot: Option<String> = None;
    // Force the first beat to attach: pretend the backstop is already due.
    let mut last_attached = Instant::now()
        .checked_sub(DISCOVERY_BACKSTOP)
        .unwrap_or_else(Instant::now);
    loop {
        // Discovery is best-effort + potentially slow (binary/file probes) → off
        // the async runtime; a failure just ships a bare heartbeat.
        let disc = tokio::task::spawn_blocking(|| discover(&DiscoveryOptions::default()))
            .await
            .ok();
        let attach = disc.as_ref().and_then(|d| {
            daemon.with_status(|s| {
                s.set_stat("installations_detected", json!(d.installations.len()));
                s.set_stat("identities_detected", json!(d.identities.len()));
            });
            let key = serde_json::to_string(&(&d.installations, &d.identities)).unwrap_or_default();
            let changed = last_snapshot.as_deref() != Some(key.as_str());
            let backstop_due = last_attached.elapsed() >= DISCOVERY_BACKSTOP;
            if changed || backstop_due {
                last_snapshot = Some(key);
                last_attached = Instant::now();
                Some(d.clone())
            } else {
                None
            }
        });
        post_heartbeat_now(&daemon, attach).await;
        tokio::time::sleep(HEARTBEAT_INTERVAL).await;
    }
}

/// Build the wire heartbeat body from the live status, optionally fold in a
/// discovery snapshot, POST it, and act on the server's release verdict.
async fn post_heartbeat_now(daemon: &Daemon, discovery: Option<DiscoveryOutput>) {
    let snap = daemon.with_status(|s| {
        s.snapshot_body(
            Some(&daemon.device_id),
            daemon.config.version(),
            &daemon.machine_id,
        )
    });
    let mut wire = heartbeat_wire_body(&snap);
    if let (Some(d), Some(obj)) = (discovery, wire.as_object_mut()) {
        obj.insert(
            "installations".into(),
            serde_json::to_value(&d.installations).unwrap_or(Value::Null),
        );
        obj.insert(
            "identities".into(),
            serde_json::to_value(&d.identities).unwrap_or(Value::Null),
        );
    }
    if let Some(resp) = daemon.api.post_heartbeat(&daemon.device_id, &wire).await {
        handle_release(daemon, resp.daemon_release);
    }
}

/// Surface the server's release verdict on the status line + last-status mirror
/// (the tray/CLI render it). The ACTION — auto-update download + swap — is M7; M4
/// only reports the verdict. Port of `handleRelease` minus `maybeAutoUpdate`.
fn handle_release(daemon: &Daemon, release: Option<Value>) {
    let verdict = release
        .as_ref()
        .and_then(|r| r.get("verdict"))
        .and_then(Value::as_str)
        .unwrap_or("ok");
    if verdict == "ok" {
        daemon.with_status(|s| {
            if s.update.is_some() {
                s.set_update(None);
            }
        });
        return;
    }
    let latest = release
        .as_ref()
        .and_then(|r| r.get("latest"))
        .and_then(Value::as_str)
        .map(String::from);
    daemon.with_status(|s| {
        s.set_update(Some(UpdateInfo {
            verdict: verdict.to_string(),
            latest,
        }))
    });
}

/// The 5s SDK-drain loop with skip-backoff: build local segments from the durable
/// queue + upload under the device secret. A held pipeline (engine down) or a
/// failed upload leaves events queued and spaces out the next attempts (≈5s → up
/// to ~30s) rather than hammering a sustained outage. Port of the `drainTick`.
async fn drain_loop(daemon: Arc<Daemon>) {
    let mut fails: u32 = 0;
    let mut skip: u32 = 0;
    loop {
        tokio::time::sleep(LOCAL_DRAIN_INTERVAL).await;
        if skip > 0 {
            skip -= 1;
            continue;
        }
        let pipeline = EnginePipeline::new(&daemon.resilient, &*daemon.embedder, &*daemon.ner);
        let mut uploader = (*daemon.api).clone();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let outcome = drain_local_queue(
            &*daemon.queue,
            &pipeline,
            &mut uploader,
            &daemon.device_id,
            daemon.config.version(),
            now_ms,
        )
        .await;
        match outcome {
            Ok(r) if !r.held => {
                if r.events > 0 {
                    daemon.with_status(|s| {
                        s.bump_stat("sdk_events_uploaded", r.events as u64);
                    });
                }
                let depth = daemon.queue.count_unsent().await;
                daemon.with_status(|s| s.set_stat("sdk_queue", json!(depth)));
                fails = 0;
            }
            // A held pipeline/upload or a queue I/O error: back off (events stay
            // durably queued; the file-scan path owns the top-level "offline").
            _ => {
                fails = (fails + 1).min(6);
                skip = fails;
            }
        }
    }
}

/// The 5-min backstop scan (FSEvents can miss things), routed through the runner
/// so it coalesces instead of stacking on a live scan. Port of the `backstop`
/// interval.
async fn backstop_loop(runner: CoalescingRunner<String>, quiescing: Arc<AtomicBool>) {
    loop {
        tokio::time::sleep(SCAN_BACKSTOP_INTERVAL).await;
        if !quiescing.load(Ordering::SeqCst) {
            runner.trigger("interval".to_string());
        }
    }
}

/// The self-healing backfill reconcile: first pass shortly after boot (so a wiped
/// scope refills without a restart), then every 30 min. Cheap when in sync — a
/// digest fetch + summariser-free parse tally that re-ships only what the server
/// is short on. Port of the `reconcileBackfill` timers.
async fn reconcile_loop(
    daemon: Arc<Daemon>,
    runner: CoalescingRunner<String>,
    quiescing: Arc<AtomicBool>,
) {
    tokio::time::sleep(RECONCILE_FIRST_DELAY).await;
    loop {
        if !quiescing.load(Ordering::SeqCst) {
            reconcile_once(&daemon, &runner, &quiescing).await;
        }
        tokio::time::sleep(RECONCILE_INTERVAL).await;
    }
}

/// One reconcile pass: parse a summariser-free per-day-session tally over the same
/// file set the scanner sees, compare to the server digest, and trigger a scan for
/// exactly the sessions it's short on.
async fn reconcile_once(
    daemon: &Daemon,
    runner: &CoalescingRunner<String>,
    quiescing: &AtomicBool,
) {
    let jobs = discover_jobs();
    let kinds: HashMap<String, ParserKind> =
        jobs.iter().map(|j| (j.path.clone(), j.kind)).collect();
    let list_files = || {
        jobs.iter()
            .map(|j| (j.path.clone(), file_mtime_secs(&j.path)))
            .collect::<Vec<_>>()
    };
    let device_id = daemon.device_id.clone();
    let mut parse_counts = |path: &str| -> Option<PerDaySession> {
        let kind = *kinds.get(path)?;
        let job = ScanJob {
            path: path.to_string(),
            kind,
        };
        let r = parse_job(&device_id, &job).ok()?;
        let mut out: PerDaySession = BTreeMap::new();
        for e in &r.events {
            // Events carry ISO-8601 UTC timestamps, so the first 10 chars are the
            // UTC day — the same bucketing the server digest uses.
            let Some(day) = e.ts.get(..10) else { continue };
            if day.is_empty() {
                continue;
            }
            *out.entry(day.to_string())
                .or_default()
                .entry(e.session_id.clone())
                .or_insert(0) += 1;
        }
        Some(out)
    };
    let mut digest = (*daemon.api).clone();
    let now_ms = chrono::Utc::now().timestamp_millis();

    let mut state = daemon.state.lock().await;
    reconcile_backfill(
        now_ms,
        list_files,
        &mut parse_counts,
        &mut digest,
        &mut *state,
        |reason: &str| {
            if !quiescing.load(Ordering::SeqCst) {
                runner.trigger(reason.to_string());
            }
        },
    )
    .await;
    let _ = save_state(&state);
}

fn file_mtime_secs(path: &str) -> f64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The throttled last-status mirror: ~every 400ms, write `last-status.json` IF the
/// snapshot changed (deduped by its device-id-carrying body, ignoring the
/// per-write `written_at`). Decoupled from the 10s heartbeat so the tray + the
/// statusline see fresh phase/progress through a long summarise pass. Port of
/// `scheduleLocalFlush` + `writeLocalStatus`.
async fn last_status_loop(daemon: Arc<Daemon>) {
    let path = home_path("last-status.json");
    let mut last: Option<String> = None;
    loop {
        let snap = daemon.with_status(|s| {
            s.snapshot_body(
                Some(&daemon.device_id),
                daemon.config.version(),
                &daemon.machine_id,
            )
        });
        let key = snap.to_string();
        if last.as_deref() != Some(key.as_str()) {
            let _ = write_last_status(&path, &snap, &now_iso());
            last = Some(key);
        }
        tokio::time::sleep(LOCAL_FLUSH_THROTTLE).await;
    }
}

/// Start the notify filesystem watcher over the AI-tool data dirs: a `.jsonl`
/// add/change schedules a debounced (1s) coalesced scan. Returns the watcher —
/// which MUST be kept alive (dropping it stops watching), so `run` holds it until
/// teardown. Port of the chokidar watcher + `scheduleScan`.
fn spawn_watcher(
    runner: CoalescingRunner<String>,
    quiescing: Arc<AtomicBool>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{Event, RecursiveMode, Watcher};

    let dirs = crate::watch::resolve_watch_dirs();
    if dirs.is_empty() {
        eprintln!("[modelstat] no AI-tool data dirs to watch (a fresh machine) — relying on the 5-min backstop scan");
        return None;
    }
    // notify's callback runs on its own thread; bridge events to async via an
    // unbounded channel (send is sync + non-blocking).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(ev) = res {
            // Only transcript writes matter (the scan re-discovers the file set).
            if ev
                .paths
                .iter()
                .any(|p| crate::watch::is_transcript_file(p))
            {
                let _ = tx.send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[modelstat] filesystem watcher unavailable ({e}) — relying on the 5-min backstop scan");
            return None;
        }
    };
    let mut watched = 0usize;
    for dir in &dirs {
        if watcher.watch(dir, RecursiveMode::Recursive).is_ok() {
            watched += 1;
        }
    }
    // The 1s debounce: collapse a burst of writes into one coalesced scan ~1s
    // after the first event.
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            tokio::time::sleep(WATCH_DEBOUNCE).await;
            while rx.try_recv().is_ok() {} // drain the burst
            if !quiescing.load(Ordering::SeqCst) {
                runner.trigger("watch".to_string());
            }
        }
    });
    eprintln!("[modelstat] watching {watched} AI-tool data directories");
    Some(watcher)
}

/// Wait for a shutdown trigger and return the process exit code: SIGINT → 130,
/// SIGTERM → 143, a lost lock race → 0 (a rival won; our supervisor adopts it).
/// A single unified wait (NOT two racing handlers) so the lock's stand-down and
/// the graceful teardown never preempt each other.
async fn wait_for_shutdown(lost_rx: oneshot::Receiver<()>) -> u8 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint =
            signal(SignalKind::interrupt()).expect("install SIGINT handler");
        let mut sigterm =
            signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => 130,
            _ = sigterm.recv() => 143,
            _ = lost_rx => 0,
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => 130,
            _ = lost_rx => 0,
        }
    }
}
