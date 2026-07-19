//! The remaining collector commands (feature §5): `reset`, `stop`/`remove`/
//! `uninstall`, `sync`, `discover`, `watch`. Ports of `cmdReset`/`cmdStop`/
//! `cmdSync`/`cmdDiscover`/`cmdWatch`. `sync`'s cold-scan preflight is the
//! no-degrade version (§9.4): it never prints a "degraded/extractive" label.

use std::sync::Arc;
use std::time::Duration;

use modelstat_daemon::processing_version::PROCESSING_VERSION;
use modelstat_daemon::runtime::{scan_session, Daemon};
use modelstat_ingest::state::{load_state, save_state};
use modelstat_ingest::{state_path, Config};
use modelstat_service::{tray, uninstall_service, Component, Scope};
use std::process::ExitCode;

/// `reset` — wipe all file cursors + stamp the current PROCESSING_VERSION so the
/// next scan re-reads every JSONL and re-summarises every session.
pub fn cmd_reset() -> ExitCode {
    let mut state = load_state();
    state.cursor.clear();
    state.processing_version = Some(PROCESSING_VERSION);
    if let Err(e) = save_state(&state) {
        eprintln!("modelstat: couldn't reset cursors: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "[modelstat] cursors reset — the daemon's next scan cycle will re-read every JSONL from the start and re-summarise every session at processing version v{PROCESSING_VERSION}."
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

    println!("  Your device pairing is still in {}", state_path().display());
    println!("  Run `modelstat` again to re-enable.");
    ExitCode::SUCCESS
}

/// `discover` — print local discovery counts; POSTs nothing (the running daemon
/// reports this on its next heartbeat).
pub fn cmd_discover() -> ExitCode {
    use modelstat_parsers::discovery::{discover, DiscoveryOptions};
    let out = discover(&DiscoveryOptions::default());
    println!(
        "→ {} installations, {} identities",
        out.installations.len(),
        out.identities.len()
    );
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
        eprintln!("usage: modelstat sync --session <id> [--session <id> …] [--file <path>] [--wait]");
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
    let daemon = Daemon::build(config, device_id, machine_id);
    scan_session(daemon, session_ids, file).await;
    println!("✓ scan complete");
    ExitCode::SUCCESS
}

/// `watch` — foreground watcher (dev convenience): an initial scan, then re-scan
/// whenever a transcript changes, plus a 5-min backstop. Runs until Ctrl-C.
pub async fn cmd_watch(config: Arc<Config>) -> ExitCode {
    let Some(device_id) = config.device_id() else {
        eprintln!("not paired — run `modelstat` first");
        return ExitCode::FAILURE;
    };
    let machine_id = modelstat_ingest::intended_device_uuid();
    let daemon = Daemon::build(config, device_id, machine_id);
    modelstat_daemon::watch::watch_forever(daemon).await;
    ExitCode::SUCCESS
}

fn is_connection_refused(e: &reqwest::Error) -> bool {
    // reqwest wraps the OS error; a refused/reset connection (no daemon) is the
    // cold-scan trigger. Fall back on the string form (portable across OSes).
    let s = e.to_string().to_lowercase();
    e.is_connect()
        || s.contains("refused")
        || s.contains("reset")
        || s.contains("actively refused")
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
