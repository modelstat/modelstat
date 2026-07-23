//! `status` + `jobs` (feature §5). `status --json` is the tray's poll API — a
//! FROZEN shape — and is strictly side-effect-free (never recovers identity on
//! 401; falls back to cached identity offline). Port of `cmdStatus`/`cmdJobs`.
//!
//! §5/§23 deltas from the TS daemon (spec is authoritative — the M6 AC grades
//! "modulo the documented deltas"): the `summarizer.model` field is GONE with
//! BYO endpoints, and `summarizer.url` is now present in BOTH self-hosted and
//! local (the loopback engine URL) — the tray reads both optionally.
//!
//! The daemon phase is LOCAL truth: the lock owner's liveness + the
//! `last-status.json` mirror, never the server's `daemon_status` echo. This
//! command runs on the same machine as the daemon — the server's copy is just
//! our own last heartbeat played back, and it was stale in BOTH directions
//! (a stuck "Shutting down" for a live daemon; a stuck "uploading" for a dead
//! one killed before its parting beat).

use std::process::ExitCode;

use modelstat_daemon::lock::{age_in_seconds, format_age};
use modelstat_daemon::supervise::daemon_health;
use modelstat_ingest::{build_fingerprint, logs_dir, state_path, Config, DeviceApi};
use modelstat_service::{service_status, Component, Scope};
use modelstat_update::{auto_update_enabled, auto_update_pinned_by_env};
use serde_json::{json, Map, Value};

use crate::util::{api_base, dashboard_url, read_local_status};

/// Wall-clock ms since the Unix epoch, for the health probe.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `last-status.json` is only as honest as the process that wrote it: when no
/// live process owns the daemon lock, the mirror is a fossil — override its
/// phase so no surface renders a dead daemon's last words ("Shutting down",
/// "uploading") as live state. Stats stay: they're history, and labelled so.
fn honest_local(local: Option<Value>, owner_alive: bool) -> Option<Value> {
    match local {
        Some(mut l) if !owner_alive => {
            if let Some(obj) = l.as_object_mut() {
                obj.insert("status".into(), json!("offline"));
                obj.insert("message".into(), json!("daemon not running"));
            }
            Some(l)
        }
        other => other,
    }
}

/// The active summarizer engine URL for this mode, or `None` for cloud. Pure —
/// no probe, no side effect (self-hosted → the stored URL; local → loopback).
fn summarizer_url(config: &Config) -> Option<String> {
    match config.summarizer_mode().as_str() {
        "self-hosted" => Some(config.self_hosted_url()),
        "local" => Some(format!(
            "http://127.0.0.1:{}",
            modelstat_daemon::engine::engine_port()
        )),
        _ => None,
    }
}

/// The `summarizer` sub-object: `{mode, url?, env_override}` — url present for
/// self-hosted + local only; model dropped (§23).
fn summarizer_obj(config: &Config) -> Value {
    let mut m = Map::new();
    m.insert("mode".into(), json!(config.summarizer_mode()));
    if let Some(url) = summarizer_url(config) {
        m.insert("url".into(), json!(url));
    }
    m.insert(
        "env_override".into(),
        json!(config.summarizer_mode_is_env_overridden()),
    );
    Value::Object(m)
}

pub async fn cmd_status(api: &DeviceApi, args: &[String]) -> ExitCode {
    let config = api.config().clone();
    let as_json = args.iter().any(|a| a == "--json");

    let paired = config.bearer().is_some() && config.device_id().is_some();
    let dashboard = dashboard_url(&config);
    let svc = service_status(Component::Daemon, Scope::User);
    let health = daemon_health(now_ms(), None);
    let local = honest_local(read_local_status(), health.owner_alive);
    let fp = build_fingerprint(config.version());

    let user_email = config.identity().and_then(|i| i.user_email);
    let mut claimed = user_email.is_some();
    let mut claim_url = config.claim_url();
    let mut claim_code = config.claim_code();
    // The daemon phase is local truth (see the module docs): a live lock owner's
    // mirror, or "offline" when no live process owns the lock.
    let daemon_status = if health.owner_alive {
        local
            .as_ref()
            .and_then(|l| l.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("starting")
            .to_string()
    } else {
        "offline".to_string()
    };

    // Side-effect-free probe: refresh claim fields when paired, but NEVER recover
    // identity here — the heartbeat loop owns 401 recovery (§5). Any error keeps
    // the cached fallbacks (offline behaves identically to a revoked secret).
    if paired {
        if let Some(secret) = config.bearer() {
            if let Ok(me) = api.fetch_device_me(&secret).await {
                claimed = me.status == "claimed";
                if me.claim_url.is_some() {
                    claim_url = me.claim_url.clone();
                }
                if me.claim_code.is_some() {
                    claim_code = me.claim_code.clone();
                }
            }
        }
    }

    let device = json!({
        "hostname": fp.hostname,
        "os_family": fp.os_family,
        "daemon_status": daemon_status,
    });
    let daemon_obj = json!({
        "running": health.owner_alive,
        "pid": health.lock.as_ref().map(|l| l.pid),
        "started_at": health.lock.as_ref().map(|l| l.started_at.clone()),
    });
    let pairing = if paired {
        json!({
            "paired": true,
            "user": user_email,
            "device": config.device_id(),
            "uuid": config.device_uuid(),
        })
    } else {
        json!({ "paired": false })
    };

    if as_json {
        let out = json!({
            "paired": paired,
            "claimed": claimed,
            "dashboard": dashboard,
            "device": device,
            "claim_url": claim_url,
            "claim_code": claim_code,
            "local": local,
            "daemon": daemon_obj,
            "service": { "running": svc.running, "hint": svc.hint },
            "pairing": pairing,
            "auto_update": { "enabled": auto_update_enabled(), "pinned_by_env": auto_update_pinned_by_env() },
            "summarizer": summarizer_obj(&config),
            "api": config.api_url(),
            "logs": logs_dir().display().to_string(),
            "state": state_path().display().to_string(),
        });
        println!("{}", serde_json::to_string(&out).unwrap());
        return ExitCode::SUCCESS;
    }

    // ── human form (TS cmdStatus tail) ──────────────────────────────────
    println!("paired:  {}", if paired { "yes" } else { "no" });
    if paired {
        println!(
            "  user:    {}",
            user_email.as_deref().unwrap_or("(unknown)")
        );
        println!("  device:  {}", config.device_id().unwrap_or_default());
        println!(
            "  uuid:    {}",
            config
                .device_uuid()
                .unwrap_or_else(|| "(not self-registered)".into())
        );
    }
    println!(
        "service: {}  ({})",
        if svc.running { "running" } else { "stopped" },
        svc.hint
    );
    match &health.lock {
        Some(lock) if health.owner_alive => println!(
            "daemon:  running (pid {}, up {})",
            lock.pid,
            format_age(age_in_seconds(&lock.started_at))
        ),
        _ => println!("daemon:  not running"),
    }
    println!("logs:    {}", logs_dir().display());
    println!("state:   {}", state_path().display());
    println!("api:     {}", config.api_url());
    let mode = config.summarizer_mode();
    let sm_endpoint = match summarizer_url(&config) {
        Some(u) if mode == "self-hosted" && !u.is_empty() => format!(" ({u})"),
        _ => String::new(),
    };
    let sm_env = if config.summarizer_mode_is_env_overridden() {
        " (env override)"
    } else {
        ""
    };
    println!("summariser: {mode}{sm_endpoint}{sm_env} — change with `modelstat mode`");
    println!(
        "auto-update: {}{}",
        if auto_update_enabled() { "on" } else { "off" },
        if auto_update_pinned_by_env() {
            " (pinned by env)"
        } else {
            ""
        }
    );
    if let Some(upd) = local.as_ref().and_then(|l| l.get("update")) {
        let verdict = upd.get("verdict").and_then(Value::as_str).unwrap_or("ok");
        if verdict != "ok" {
            let what = if verdict == "upgrade_required" {
                "REQUIRED"
            } else {
                "available"
            };
            let latest = upd.get("latest").and_then(Value::as_str).unwrap_or("?");
            println!("update:  {what} — latest {latest} (run `modelstat upgrade`)");
        }
    }

    println!();
    if !paired {
        println!("usage:   not paired yet — run `modelstat`");
        return ExitCode::SUCCESS;
    }
    println!("usage:   full numbers in your dashboard:");
    println!("  {dashboard}");
    print_local_pipeline(local.as_ref(), false);
    let _ = api_base(&config);
    ExitCode::SUCCESS
}

pub async fn cmd_jobs(api: &DeviceApi, args: &[String]) -> ExitCode {
    let config = api.config().clone();
    let as_json = args.iter().any(|a| a == "--json");
    let paired = config.bearer().is_some() && config.device_id().is_some();
    let dashboard = format!("{}/dashboard/jobs", api_base(&config));

    if !paired {
        if as_json {
            println!("{}", json!({ "paired": false, "reason": "not_paired" }));
        } else {
            println!("not paired yet — run `modelstat` first");
        }
        return ExitCode::SUCCESS;
    }

    let health = daemon_health(now_ms(), None);
    let local = honest_local(read_local_status(), health.owner_alive);
    let phase = local
        .as_ref()
        .and_then(|l| l.get("status"))
        .and_then(Value::as_str);
    let queue = local
        .as_ref()
        .and_then(|l| l.get("queue_size"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let stats = local
        .as_ref()
        .and_then(|l| l.get("stats"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    if as_json {
        let out = json!({
            "paired": true,
            "dashboard": dashboard,
            "phase": phase,
            "queue_size": queue,
            "stats": stats,
        });
        println!("{}", serde_json::to_string(&out).unwrap());
        return ExitCode::SUCCESS;
    }

    println!("jobs:    full job queue + ledger in your dashboard:");
    println!("  {dashboard}");
    println!();
    println!("local pipeline (this device):");
    print_local_pipeline(local.as_ref(), true);
    ExitCode::SUCCESS
}

/// Shared "local pipeline" tail (`phase`, optional `queue`, then `stats`
/// entries) — the small differences between status/jobs are `with_queue`.
fn print_local_pipeline(local: Option<&Value>, with_queue: bool) {
    let Some(local) = local else {
        println!("  (no local heartbeat yet — is the daemon running?)");
        return;
    };
    if let Some(phase) = local.get("status").and_then(Value::as_str) {
        let msg = local.get("message").and_then(Value::as_str);
        match msg {
            Some(m) => println!("  phase: {phase} — {m}"),
            None => println!("  phase: {phase}"),
        }
    }
    if with_queue {
        let queue = local.get("queue_size").and_then(Value::as_i64).unwrap_or(0);
        println!("  queue: {queue}");
    }
    if let Some(stats) = local.get("stats").and_then(Value::as_object) {
        for (k, v) in stats {
            println!("  {k}: {}", value_scalar(v));
        }
    }
}

/// Render a stat value without JSON quotes (numbers + strings alike).
fn value_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
