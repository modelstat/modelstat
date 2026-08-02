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

use modelstat_ingest::accounts;
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
/// Rewrite the last-status mirror at least this often even when nothing changed,
/// so its `written_at` is a real liveness signal (see [`last_status_loop`]).
const MIRROR_LIVENESS_REWRITE: Duration = Duration::from_secs(10);

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
    // Timestamped logging from the first line: this process runs for days under a
    // supervisor, and its stderr is a log file someone reads long after the fact.
    // Unconditional — running `modelstat start` by hand in a terminal is running
    // the daemon, and must log identically to the supervised one.
    modelstat_log::init_service();

    // ── Enrollment guard ────────────────────────────────────────────────────
    let (Some(_bearer), Some(device_id)) = (config.bearer(), config.device_id()) else {
        modelstat_log::log_error!(
            "not enrolled — run `modelstat` (or `modelstat self-register`) first"
        );
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
            modelstat_log::log_info!(
                "daemon is already running — PID {}, started {} ago, daemon {}.\n\
                 \x20 → to stop it:          kill {}\n\
                 \x20 → to force-replace it: modelstat start --force",
                owner.pid,
                format_age(age_sec),
                owner.daemon_version,
                owner.pid
            );
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            modelstat_log::log_error!("could not acquire the daemon lock: {e}");
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
    // Handles kept: shutdown aborts these AFTER the drain so they broadcast the
    // live truth for exactly as long as it is the truth, and not a beat longer.
    let heartbeat_task = tokio::spawn(heartbeat_loop(daemon.clone()));

    // ── Throttled last-status mirror (tray/statusline read this file) ───────
    // BEFORE any scan: a fresh boot must immediately replace whatever a
    // predecessor left in `last-status.json` — spawning the mirror only after
    // the first scan (the old shape) let a stale "Shutting down" sit there for
    // the entire (possibly hours-long) startup backfill.
    let mirror_task = tokio::spawn(last_status_loop(daemon.clone()));

    // ── On-device model self-heal (background) ──────────────────────────────
    // `connect` pre-warms the NER + BGE weights, but it gets ONE shot: a network
    // blip there used to leave a model missing forever, with nothing to notice or
    // fix it. This retries until it lands and swaps the real model in without a
    // restart. Backgrounded — a ~560 MB download must never gate ingestion, and
    // on the overwhelmingly common path (models present) it costs two `stat`s.
    let heal_task = tokio::spawn(heal_models(daemon.clone()));

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
            modelstat_log::log_info!(
                "processing pipeline v{} → v{} — wiped file cursors so every session \
                 is re-processed by the new pipeline",
                pv.from,
                pv.to
            );
            let _ = save_state(&state);
        }
    }

    // ── The racing-daemon convergence recheck (feature §21.7) ───────────────
    // Wait out the recheck window BEFORE the first scan: if a rival out-renamed
    // our lock write, stand down NOW — and stand down SILENTLY. The winner owns
    // the device row and the status mirror; a parting "offline" heartbeat or
    // mirror write from us would clobber its truth. (The old shape raced the
    // recheck against the signal wait, which only ran after the awaited first
    // scan — so a losing daemon spent the whole startup backfill double-scanning
    // and double-uploading before it ever noticed it had lost.)
    tokio::time::sleep(Duration::from_millis(LOCK_RECHECK_MS)).await;
    {
        let current = read_daemon_lock(&lock_path);
        if check_lock_ownership(
            std::process::id() as i64,
            current.as_ref(),
            is_process_alive,
        ) == OwnershipCheck::Lost
        {
            modelstat_log::log_info!("another daemon took the lock during boot — standing down");
            heartbeat_task.abort();
            mirror_task.abort();
            // The winner runs its own heal; two processes downloading into the
            // same cache dir would race on the `.partial` files.
            heal_task.abort();
            if let Some(r) = &receiver {
                r.abort();
            }
            return ExitCode::SUCCESS;
        }
    }

    // ── First scan (awaited) ────────────────────────────────────────────────
    // Discovery rides the primed heartbeat, so go straight to scanning.
    runner.trigger("startup".to_string());
    runner.idle().await;

    // ── Filesystem watcher + 5-min backstop + reconcile timers ──────────────
    let _watcher = spawn_watcher(runner.clone(), quiescing.clone());
    tokio::spawn(backstop_loop(runner.clone(), quiescing.clone()));
    tokio::spawn(reconcile_loop(
        daemon.clone(),
        runner.clone(),
        quiescing.clone(),
    ));

    // ── Park until a signal, then drain, THEN go offline ────────────────────
    let code = wait_for_shutdown().await;
    quiescing.store(true, Ordering::SeqCst);
    if let Some(r) = &receiver {
        r.abort();
    }
    // Drain the in-flight scan FIRST, without touching the phase: until the
    // drain completes this daemon is still scanning/uploading, and the heartbeat
    // + mirror loops keep reporting that truth. (The old order flipped the phase
    // to Offline "Shutting down" up front, so a daemon spending an hour
    // finishing a big backfill broadcast "Shutting down" the whole time — a
    // live, actively uploading collector every surface called dead.) A second
    // signal skips the wait for people who really mean "now".
    tokio::select! {
        _ = runner.idle() => {}
        _ = wait_for_shutdown() => {
            modelstat_log::log_warn!("second signal — exiting without draining the in-flight scan");
        }
    }
    // NOW offline is true: stop the broadcasters, say it once, and leave. The
    // heal's partial download survives on disk — the next boot resumes it.
    heartbeat_task.abort();
    mirror_task.abort();
    heal_task.abort();
    daemon.with_status(|s| s.set_phase(Phase::Offline, "Stopped"));
    post_heartbeat_now(&daemon, None).await;
    let snap = daemon.with_status(|s| {
        s.refresh_auto_update();
        s.snapshot_body(
            Some(&daemon.device_id),
            daemon.config.version(),
            &daemon.machine_id,
        )
    });
    let _ = write_last_status(&home_path("last-status.json"), &snap, &now_iso());
    remove_lock_if_owned(&lock_path, std::process::id() as i64);
    ExitCode::from(code)
}

/// Download any missing on-device model, then swap the real one in for the
/// fail-safe placeholder the daemon booted with.
///
/// Only the models that actually landed are rebuilt: a `heal` that fetched BGE
/// must not touch a NER handle that is already the real thing (rebuilding it
/// would re-mmap ~430 MB for nothing).
async fn heal_models(daemon: Arc<Daemon>) {
    for entry in crate::engine::heal_missing_models().await {
        // Exhaustive by design — a new ModelSlot variant fails to compile here
        // until it is given a loader.
        match entry.slot {
            crate::engine::ModelSlot::Ner => daemon.ner.set(crate::engine::build_ner()),
            crate::engine::ModelSlot::Embedder => {
                daemon.embedder.set(crate::engine::build_embedder())
            }
        }
        modelstat_log::log_info!("{} loaded — no restart needed", entry.label);
    }
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
        format!(
            "self-hosted summarizer at {}",
            daemon.config.self_hosted_url()
        )
    } else {
        "local summarizer (Qwen3.5-4B)".to_string()
    };
    // Smoke the RAW engine (fast-fail): a down engine surfaces NOW as a loud held
    // status instead of hanging boot on the resilient wrapper's retries.
    let report = modelstat_pipeline::preflight(&label, daemon.resilient.engine()).await;
    if report.available {
        modelstat_log::log_info!("summariser preflight ok: {}", report.message);
        daemon.with_status(|s| s.set_message(format!("summariser ready: {label}")));
    } else {
        modelstat_log::log_warn!("summariser unavailable — {}", report.message);
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
        // Remember which account is logged in, for the scan to name on each
        // session it ships. Folded on EVERY beat, not just the ones that attach a
        // snapshot to the heartbeat: the two cadences are unrelated, and the
        // window `observed_since` measures is only correct if every reading
        // updates it. Best-effort — a failed write just means nothing is stamped
        // and the server infers, exactly as before.
        if let Some(d) = disc.as_ref() {
            let detected: Vec<(String, String)> = d
                .identities
                .iter()
                .map(|i| (i.provider.clone(), i.provider_account_id.clone()))
                .collect();
            let stored = accounts::load_accounts();
            let folded = accounts::fold_discovery(&stored, &detected, now_ms());
            if folded != stored {
                if let Err(e) = accounts::save_accounts(&folded) {
                    eprintln!("accounts: could not persist the logged-in account: {e}");
                }
            }
        }

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
        s.refresh_auto_update();
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
/// (tray/CLI render it) AND act on it (§13): auto-update off → a one-time nudge;
/// on → a DETACHED `_apply-update` helper that downloads + swaps both binaries.
/// Port of `handleRelease` + `maybeAutoUpdate`.
fn handle_release(daemon: &Daemon, release: Option<Value>) {
    use modelstat_update::{maybe_auto_update, AutoUpdateStep, DaemonRelease};

    let rel: DaemonRelease = release
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let verdict = rel.verdict.clone().unwrap_or_else(|| "ok".to_string());

    // Mirror the verdict onto the status line first (unchanged behaviour).
    if verdict == "ok" {
        daemon.with_status(|s| {
            if s.update.is_some() {
                s.set_update(None);
            }
        });
        return;
    }
    daemon.with_status(|s| {
        s.set_update(Some(UpdateInfo {
            verdict: verdict.clone(),
            latest: rel.latest.clone(),
        }))
    });

    // Decide (verdict guard → in-progress marker → per-process dedup → on/off).
    let step = {
        let mut handled = daemon
            .handled_updates
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        maybe_auto_update(&rel, &mut handled, now_ms())
    };
    match step {
        AutoUpdateStep::Skip => {}
        AutoUpdateStep::Nudge(note) => modelstat_log::log_info!("{note}"),
        AutoUpdateStep::Proceed { target } => {
            let t = target.unwrap_or_default();
            let label = if t.is_empty() { "latest" } else { &t };
            modelstat_log::log_info!("auto-updating to {label} — downloading + swapping both binaries; the service restarts on the new build");
            spawn_detached_update(&t);
        }
    }
}

/// Milliseconds since the Unix epoch (the marker/decision clock).
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Spawn `modelstat _apply-update <target>` DETACHED — a separate process that
/// outlives this daemon, because the swap it performs runs `_install-service`,
/// which bounces (and kills) this very daemon mid-upgrade. A new process
/// group/session keeps the updater alive through that bounce (§13 — the TS
/// daemon likewise ran the update in a detached worker).
fn spawn_detached_update(target: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("_apply-update")
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let _ = cmd.spawn();
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
        let embedder = daemon.embedder.get();
        let ner = daemon.ner.get();
        let pipeline = EnginePipeline::new(&daemon.resilient, &*embedder, &*ner);
        let mut uploader = (*daemon.api).clone();
        let now_ms = chrono::Utc::now().timestamp_millis();
        // Fresh per pass, like the scan's — the resolver caches cwd→context, and a
        // long-lived cache would go stale against branch switches between drains.
        let mut git = modelstat_parsers::RealGitEnrichment::new();
        let outcome = drain_local_queue(
            &*daemon.queue,
            &pipeline,
            &mut uploader,
            &daemon.device_id,
            daemon.config.version(),
            now_ms,
            Some(&mut git as &mut (dyn modelstat_parsers::GitEnrichment + Send)),
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

/// The throttled last-status mirror: ~every 400ms, write `last-status.json` when
/// the snapshot changed — and at least every [`MIRROR_LIVENESS_REWRITE`] even
/// when it hasn't, so `written_at` doubles as the LOCAL liveness beat. The
/// supervisor probe (`_daemon-health`) and the tray judge daemon liveness by
/// that freshness; the old changed-only dedupe let a healthy-but-idle daemon's
/// mirror age past the probe's threshold, which then "replaced" (force-killed)
/// a perfectly live collector. Decoupled from the 10s network heartbeat so the
/// tray + statusline see fresh phase/progress through a long summarise pass.
/// Port of `scheduleLocalFlush` + `writeLocalStatus`.
async fn last_status_loop(daemon: Arc<Daemon>) {
    let path = home_path("last-status.json");
    let mut last: Option<String> = None;
    let mut last_write = Instant::now();
    loop {
        let snap = daemon.with_status(|s| {
            s.refresh_auto_update();
            s.snapshot_body(
                Some(&daemon.device_id),
                daemon.config.version(),
                &daemon.machine_id,
            )
        });
        let key = snap.to_string();
        if last.as_deref() != Some(key.as_str()) || last_write.elapsed() >= MIRROR_LIVENESS_REWRITE
        {
            let _ = write_last_status(&path, &snap, &now_iso());
            last = Some(key);
            last_write = Instant::now();
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
        modelstat_log::log_info!(
            "no AI-tool data dirs to watch (a fresh machine) — relying on the 5-min backstop scan"
        );
        return None;
    }
    // notify's callback runs on its own thread; bridge events to async via an
    // unbounded channel (send is sync + non-blocking).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(ev) = res {
            // Only transcript writes matter (the scan re-discovers the file set).
            if ev.paths.iter().any(|p| crate::watch::is_transcript_file(p)) {
                let _ = tx.send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            modelstat_log::log_warn!(
                "filesystem watcher unavailable ({e}) — relying on the 5-min backstop scan"
            );
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
    modelstat_log::log_info!("watching {watched} AI-tool data directories");
    Some(watcher)
}

/// Wait for a shutdown signal and return the process exit code: SIGINT → 130,
/// SIGTERM → 143. (The lost-lock stand-down no longer rides this wait — the
/// ownership recheck resolves inline before the first scan.)
async fn wait_for_shutdown() -> u8 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => 130,
            _ = sigterm.recv() => 143,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        130
    }
}
