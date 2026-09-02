//! The remaining collector commands (feature §5): `reset`, `stop`/`remove`/
//! `uninstall`, `sync`, `discover`, `watch`. Ports of `cmdReset`/`cmdStop`/
//! `cmdSync`/`cmdDiscover`/`cmdWatch`. `sync`'s cold-scan preflight is the
//! no-degrade version (§9.4): it never prints a "degraded/extractive" label.

use std::sync::Arc;
use std::time::Duration;

use modelstat_daemon::lock::{
    daemon_lock_path, is_process_alive, read_daemon_lock, terminate_process, terminate_process_hard,
};
use modelstat_daemon::runtime::{scan_session, Daemon};
use modelstat_daemon::supervise::{daemon_health, SuperviseDecision};
use modelstat_ingest::state::{load_state, save_state};
use modelstat_ingest::{state_path, Config};
use modelstat_service::{
    install_service, service_pid, stop_service, tray, uninstall_service, Component, Scope,
};
use std::process::ExitCode;

/// `reset` — wipe all file cursors + stamp every pipeline aspect at its current
/// version so the next scan re-reads every transcript and re-processes every
/// session (without the boot reconcile mistaking the wipe for a stale install).
pub fn cmd_reset() -> ExitCode {
    let mut state = load_state();
    state.cursor.clear();
    for (aspect, v) in modelstat_ingest::processing::aspect_versions() {
        state.processing_aspects.insert(aspect.to_string(), v);
    }
    // Every cursor just went, so the next scan re-reads the world regardless of
    // any aspect's outstanding mandate. Clearing the markers keeps the status
    // surfaces honest: nothing is owed that this reset has not already ordered.
    state.processing_rescans.clear();
    if let Err(e) = save_state(&state) {
        eprintln!("modelstat: couldn't reset cursors: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "cursors reset — the daemon's next scan cycle will re-read every transcript from the start and re-process every session."
    );
    println!("  If the daemon is running, kick it now with: modelstat stop && modelstat start");
    ExitCode::SUCCESS
}

/// `stop` / `remove` / `uninstall` — tear down the daemon service (+ local
/// engine, tray, statusline; best-effort). Identity is PRESERVED and said so.
pub fn cmd_stop() -> ExitCode {
    if let Err(e) = uninstall_service(Component::Daemon, Scope::User) {
        eprintln!("✗ {e}");
        return ExitCode::FAILURE;
    }
    println!("✓ service stopped and uninstalled");
    // Local engine service (armed only in local mode) — best-effort.
    let _ = uninstall_service(Component::Summarizer, Scope::User);

    // Tray (best-effort).
    tray::uninstall_tray_autostart();
    let app = tray::tray_app_path();
    let tray_removed = if app.exists() {
        std::fs::remove_dir_all(&app).is_ok()
    } else {
        true
    };
    if tray_removed {
        println!("✓ menu-bar tray removed");
    } else {
        println!("  (couldn't fully remove the tray)");
    }

    // Statusline (best-effort).
    use modelstat_daemon::claude_settings::{remove_statusline, RemoveResult};
    match remove_statusline() {
        RemoveResult::Removed { restored: true } => {
            println!("✓ statusline removed (your previous statusLine was restored)")
        }
        RemoveResult::Removed { restored: false } => {
            println!("✓ statusline removed from Claude Code settings")
        }
        RemoveResult::Error(message) => {
            println!("  (couldn't remove the statusline: {message})")
        }
        _ => {}
    }

    // PATH wiring (§3.3) — the same class of thing as the tray + statusline: a
    // file we wrote OUTSIDE our own home, so teardown owns removing it. Leaving
    // it would also strand a `. "~/.modelstat/env"` line in the user's startup
    // file that errors on every new shell once that home is gone.
    let bin = modelstat_service::bin_dir();
    match modelstat_service::path_env::remove_from_path(&bin, Scope::User) {
        Ok(removed) => {
            for f in &removed {
                println!("✓ PATH entry removed from {}", f.display());
            }
        }
        Err(e) => println!("  (couldn't remove the PATH entry: {e})"),
    }

    println!(
        "  Your device pairing is still in {}",
        state_path().display()
    );
    // Absolute, because the PATH entry above is gone in any new shell.
    println!(
        "  Run `{}` again to re-enable.",
        bin.join("modelstat").display()
    );
    ExitCode::SUCCESS
}

/// `_ensure-daemon` — converge toward "exactly one live daemon, run by the
/// managed service" (§15). The service manager (launchd/systemd) is the daemon's
/// ONLY supervisor; this command never runs the daemon itself — it reconciles
/// the managed service and lets the manager do the running. The tray's boot +
/// 30s watchdog shell this instead of spawning `modelstat start` as their own
/// child (two supervisors is how a machine ends up with a tray-parented daemon
/// nobody restarts). Decision (supervise::daemon_health):
///   adopt   → a live daemon owns the lock and its status mirror is fresh — do
///             nothing (whoever runs it, it's healthy).
///   spawn   → no live owner. A managed daemon that's seconds into boot hasn't
///             written its lock yet — leave it to finish rather than bouncing
///             it; otherwise reinstall + start the service (idempotent).
///   replace → a live owner stopped updating its mirror: SIGTERM it, escalate to
///             SIGKILL after a grace, then reinstall + start the service.
pub fn cmd_ensure_daemon(version: &str) -> ExitCode {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let health = daemon_health(now_ms, Some(version));
    match health.decision {
        SuperviseDecision::Adopt => {
            let pid = health.lock.as_ref().map(|l| l.pid).unwrap_or(0);
            // Say which question was answered. This line reports SUPERVISION
            // ("is exactly one live daemon running?"), and it used to read
            // "nothing to do" — printed, in the field, beside a daemon whose
            // own status said "scanning — 3,398 session files left". A verdict
            // that does not name its subject is indistinguishable from a claim
            // that there is no work.
            modelstat_log::log_info!(
                "daemon healthy (pid {pid}) — no supervision action needed \
                 (this says nothing about scan progress; see `modelstat status`)"
            );
            ExitCode::SUCCESS
        }
        SuperviseDecision::Spawn => {
            if let Some(pid) = service_pid(Component::Daemon, Scope::User) {
                modelstat_log::log_info!(
                    "managed daemon (pid {pid}) is booting — leaving it to finish"
                );
                return ExitCode::SUCCESS;
            }
            reload_daemon_service()
        }
        SuperviseDecision::Replace => {
            if let Some(lock) = &health.lock {
                modelstat_log::log_warn!(
                    "daemon pid {} stopped updating its status (age {:?}ms) — replacing it",
                    lock.pid,
                    health.status_age_ms
                );
                terminate_owner(lock.pid);
            }
            reload_daemon_service()
        }
    }
}

/// SIGTERM `pid` and wait for it to exit; escalate to SIGKILL after ~10s.
fn terminate_owner(pid: i64) {
    terminate_process(pid);
    for _ in 0..40 {
        if !is_process_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    modelstat_log::log_warn!("daemon pid {pid} ignored SIGTERM for 10s — SIGKILL");
    terminate_process_hard(pid);
    std::thread::sleep(Duration::from_millis(500));
}

/// Reinstall + start the managed daemon service, loudly.
fn reload_daemon_service() -> ExitCode {
    match install_service(Component::Daemon, Scope::User) {
        Ok(r) => {
            modelstat_log::log_info!("daemon service reloaded ({})", r.path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            modelstat_log::log_error!("couldn't reload the daemon service: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `_stop-daemon` — stop the collector WITHOUT uninstalling anything: stop the
/// managed service instance (it stays installed; the next `_ensure-daemon` or
/// login starts it again) and SIGTERM any non-managed lock owner. The tray's
/// Pause/Quit verb.
pub fn cmd_stop_daemon() -> ExitCode {
    if let Err(e) = stop_service(Component::Daemon, Scope::User) {
        modelstat_log::log_error!("couldn't stop the daemon service: {e}");
        return ExitCode::FAILURE;
    }
    // An owner running outside the service manager (a dev's terminal daemon, an
    // orphan from an old tray) doesn't hear the service stop — terminate it too.
    if let Some(lock) = read_daemon_lock(&daemon_lock_path()) {
        if is_process_alive(lock.pid) {
            terminate_process(lock.pid);
        }
    }
    modelstat_log::log_info!("daemon stopped (service stays installed — `modelstat _ensure-daemon` or the next login restarts it)");
    ExitCode::SUCCESS
}

/// `discover` — print local discovery counts; POSTs nothing (the running daemon
/// reports this on its next heartbeat).
pub fn cmd_discover() -> ExitCode {
    use modelstat_parsers::discovery::{discover, DiscoveryOptions};
    let out = discover(&DiscoveryOptions::default());
    println!(
        "→ {} installations, {} identities, {} handles",
        out.installations.len(),
        out.identities.len(),
        out.handles.len()
    );
    // The handles are what the server folds onto THIS device's person — worth
    // seeing before they travel: a login is a name, never a secret.
    for h in &out.handles {
        match &h.display_name {
            Some(name) => println!(
                "  {} {} ({name}) via {}",
                h.provider, h.handle, h.detection_source
            ),
            None => println!("  {} {} via {}", h.provider, h.handle, h.detection_source),
        }
    }
    println!("(the running daemon reports this to the server on its next heartbeat — `modelstat discover` is read-only)");
    ExitCode::SUCCESS
}

/// The loopback control port (`MODELSTAT_LOCAL_INGEST_PORT`, else 4319).
fn control_port() -> u16 {
    std::env::var("MODELSTAT_LOCAL_INGEST_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4319)
}

/// `sync --session <id>… [--file <path>] [--wait] [--port <n>]` — warm-daemon
/// first: POST the loopback control scan; ECONNREFUSED → cold in-process scan.
pub async fn cmd_sync(config: Arc<Config>, args: &[String]) -> ExitCode {
    let session_ids: Vec<String> = flag_values(args, "--session");
    let file = flag_value(args, "--file");
    if session_ids.is_empty() && file.is_none() {
        eprintln!(
            "usage: modelstat sync --session <id> [--session <id> …] [--file <path>] [--wait]"
        );
        eprintln!("  (the background daemon ingests everything on its own; use sync to force one session now)");
        return ExitCode::FAILURE;
    }
    let wait = args.iter().any(|a| a == "--wait");
    let port = flag_value(args, "--port")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or_else(control_port);

    // Warm path: POST /v1/control/scan (5s, or 600s with --wait).
    let mut body = serde_json::Map::new();
    if !session_ids.is_empty() {
        body.insert("session_ids".into(), serde_json::json!(session_ids));
    }
    if let Some(f) = &file {
        body.insert("file".into(), serde_json::json!(f));
    }
    body.insert("wait".into(), serde_json::json!(wait));
    let timeout = Duration::from_millis(if wait { 600_000 } else { 5_000 });
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/control/scan"))
        .json(&serde_json::Value::Object(body))
        .timeout(timeout)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            println!(
                "{}",
                if wait {
                    "✓ daemon force-scanned the session"
                } else {
                    "✓ asked the running daemon to force-scan the session"
                }
            );
            return ExitCode::SUCCESS;
        }
        Ok(r) => {
            let status = r.status().as_u16();
            let msg = r.text().await.unwrap_or_default();
            eprintln!("✗ daemon control scan failed ({status}): {msg}");
            return ExitCode::FAILURE;
        }
        Err(e) if !is_connection_refused(&e) => {
            eprintln!("✗ daemon control scan failed (0): {e}");
            return ExitCode::FAILURE;
        }
        Err(_) => { /* no daemon listening — fall through to the cold scan */ }
    }

    // Cold path: scan in-process (requires pairing to attribute events).
    println!("no running daemon on the control port — scanning in-process…");
    let Some(device_id) = config.device_id() else {
        eprintln!("not paired — run `modelstat` first, then retry `modelstat sync`");
        return ExitCode::FAILURE;
    };
    let machine_id = modelstat_ingest::intended_device_uuid();
    let daemon = match Daemon::build(config, device_id, machine_id) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("✗ could not open the upload spool: {e}");
            return ExitCode::FAILURE;
        }
    };
    scan_session(daemon.clone(), session_ids, file).await;
    // Scanning only redacts and queues. In the daemon a background loop does the
    // sending; here there is no daemon (that is why we are on the cold path), so
    // this process has to ship what it just produced or it would go nowhere.
    let sent =
        modelstat_daemon::uploader::drain_until_quiet(&daemon.spool, &*daemon.api, &mut ()).await;
    if sent.held {
        // Loud, and specific about what it means: the work is safe, it just has
        // not left yet.
        eprintln!(
            "✗ scan complete, but the server would not take {} queued batch(es) — \
             they are saved on disk and will be sent by the daemon (or the next \
             `modelstat sync`) with no reprocessing",
            sent.depth.batches
        );
        return ExitCode::FAILURE;
    }
    if sent.rejected > 0 {
        // Not an outage, and not success either: the server will not take these as
        // written. Saying "✓ scan complete" here is what let a contract mismatch
        // look like a healthy machine for 14 hours.
        eprintln!(
            "✗ scan complete, but the server REFUSED {} queued batch(es) on their \
             content — they are saved on disk and nothing is lost, but they will not \
             be accepted as written. This is a daemon/server contract mismatch; \
             please report it with `modelstat --version`.",
            sent.rejected
        );
        return ExitCode::FAILURE;
    }
    println!("✓ scan complete — {} events uploaded", sent.events_uploaded);
    ExitCode::SUCCESS
}

/// `watch` — foreground watcher (dev convenience): an initial scan, then re-scan
/// whenever a transcript changes, plus a 5-min backstop. Runs until Ctrl-C.
pub async fn cmd_watch(config: Arc<Config>) -> ExitCode {
    // A foreground daemon is still a daemon: same timestamped log shape.
    modelstat_log::init_service();

    let Some(device_id) = config.device_id() else {
        modelstat_log::log_error!("not paired — run `modelstat` first");
        return ExitCode::FAILURE;
    };
    let machine_id = modelstat_ingest::intended_device_uuid();
    let daemon = match Daemon::build(config, device_id, machine_id) {
        Ok(d) => d,
        Err(e) => {
            modelstat_log::log_error!("could not open the upload spool: {e}");
            return ExitCode::FAILURE;
        }
    };
    modelstat_daemon::watch::watch_forever(daemon).await;
    ExitCode::SUCCESS
}

fn is_connection_refused(e: &reqwest::Error) -> bool {
    // reqwest wraps the OS error; a refused/reset connection (no daemon) is the
    // cold-scan trigger. Fall back on the string form (portable across OSes).
    let s = e.to_string().to_lowercase();
    e.is_connect() || s.contains("refused") || s.contains("reset") || s.contains("actively refused")
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            return Some(v.to_string());
        }
    }
    None
}

/// Collect every value of a repeatable flag (`--session a --session b`,
/// `--session=a`). A value starting with `-` is NOT consumed. TS `flagValues`.
fn flag_values(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let eq_prefix = format!("{name}=");
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == name {
            if let Some(v) = args.get(i + 1) {
                if !v.starts_with('-') {
                    out.push(v.clone());
                    i += 2;
                    continue;
                }
            }
        } else if let Some(v) = a.strip_prefix(&eq_prefix) {
            out.push(v.to_string());
        }
        i += 1;
    }
    out
}
