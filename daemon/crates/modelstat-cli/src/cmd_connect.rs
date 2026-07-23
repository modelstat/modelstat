//! `connect` / `reinstall` / *(default)* — the idempotent onboarding flow
//! (feature §5, §3.3). Port of `cmdConnect`, with the documented §5/§23 deltas
//! (spec is authoritative; the M6 AC grades "modulo the documented deltas"):
//!
//! - Tray is PREBUILT only — the on-device Swift compile + its `tray_build_*`
//!   events are dropped (§5.2).
//! - The consent gate is enforced: a FRESH non-interactive install without
//!   `--mode` exits 1; `--mode self-hosted` without a resolvable URL is fatal in
//!   non-interactive runs — the silent cloud fallback is removed (§3.3/§23).
//! - `summarizer_mode` emits `{mode, url?}` — the `model` field is gone (§23);
//!   plus additive `summarizer_service_installed`/`_failed` events.
//! - MCP wiring runs the EMBEDDED `mcp wire` in-process — no `npx` subprocess.
//! - `--system` (new) installs system-scope services.

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use modelstat_ingest::{identity_path, DeviceApi, DeviceMeError};
use modelstat_service::{install_service, path_env, tray, uninstall_service, Component, Scope};
use serde_json::{json, Map, Value};

use crate::util::{api_base, has_terminal, open_browser};

/// Parsed `connect` flags (feature §5).
pub struct ConnectOpts {
    pub json: bool,
    pub no_browser: bool,
    pub fresh: bool,
    pub yes: bool,
    pub mode: Option<String>,
    pub url: Option<String>,
    pub system: bool,
}

pub fn parse_connect_opts(args: &[String]) -> ConnectOpts {
    let has = |f: &str| args.iter().any(|a| a == f);
    ConnectOpts {
        json: has("--json"),
        no_browser: has("--no-browser"),
        fresh: has("--fresh"),
        yes: has("--yes") || has("-y"),
        mode: flag_value(args, "--mode"),
        url: flag_value(args, "--url"),
        system: has("--system"),
    }
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

// ── human step/ok/warn (suppressed in --json) + the NDJSON emitter ──────
fn step(json: bool, msg: &str) {
    if !json {
        println!("\x1b[1;36m▸\x1b[0m {msg}");
    }
}
fn ok_line(json: bool, msg: &str) {
    if !json {
        println!("  \x1b[32m✓\x1b[0m {msg}");
    }
}
fn warn_line(json: bool, msg: &str) {
    if !json {
        println!("  \x1b[33m⚠\x1b[0m {msg}");
    }
}

/// Build the schema-v1 NDJSON envelope `{v:1, ts, event, …fields}`. Pure — `emit`
/// stamps the clock and prints; this is the unit-testable core so the onboarding
/// stream's wire contract (what the tray + harness consume) stays locked.
fn connect_line(event: &str, ts: u64, fields: Value) -> Value {
    let mut m = Map::new();
    m.insert("v".into(), json!(1));
    m.insert("ts".into(), json!(ts));
    m.insert("event".into(), json!(event));
    if let Value::Object(f) = fields {
        for (k, v) in f {
            m.insert(k, v);
        }
    }
    Value::Object(m)
}

/// One NDJSON line `{v:1, ts, event, …fields}` (schema v1) — only in `--json`.
fn emit(json: bool, event: &str, fields: Value) {
    if !json {
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    println!(
        "{}",
        serde_json::to_string(&connect_line(event, ts, fields)).unwrap()
    );
}

fn now_bak_suffix() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("bak-{ms}")
}

pub async fn cmd_connect(api: &DeviceApi, opts: ConnectOpts) -> ExitCode {
    let config = api.config().clone();
    let j = opts.json;
    let scope = if opts.system {
        Scope::System
    } else {
        Scope::User
    };
    // "Fresh install" for the consent gate = no identity on disk yet (a re-run
    // reuses the already-consented mode). Captured BEFORE step 1 registers.
    let fresh_install = !identity_path().exists();

    // ── 1. Identity ─────────────────────────────────────────────────────
    if opts.fresh && identity_path().exists() {
        step(j, "`--fresh` passed — re-registering this device");
        let bak = identity_path().with_extension(format!("json.{}", now_bak_suffix()));
        if std::fs::rename(identity_path(), &bak).is_ok() {
            warn_line(j, &format!("old identity moved to {}", bak.display()));
        }
        if crate::cmd_self_register(api).await != ExitCode::SUCCESS {
            return ExitCode::FAILURE;
        }
    } else if config.device_uuid().is_none()
        || config.bearer().is_none()
        || config.device_id().is_none()
    {
        step(j, "Registering this device with modelstat.ai");
        if crate::cmd_self_register(api).await != ExitCode::SUCCESS {
            return ExitCode::FAILURE;
        }
    } else {
        step(j, "Re-using existing device identity");
        ok_line(
            j,
            &format!("device {}", config.device_id().unwrap_or_default()),
        );
        ok_line(j, &format!("identity file {}", identity_path().display()));
    }

    // Probe claimed state (refresh claim fields); a 401 offers re-register when
    // interactive. Side-effectful here (unlike `status`) — this is onboarding.
    let mut claimed = false;
    if let Some(secret) = config.bearer() {
        match api.fetch_device_me(&secret).await {
            Ok(me) => {
                claimed = me.status == "claimed";
                if me.claim_url.is_some() {
                    config.set_claim_url(me.claim_url.clone());
                }
                if me.claim_code.is_some() {
                    config.set_claim_code(me.claim_code.clone());
                }
            }
            Err(DeviceMeError::Unauthorized) => {
                let re = !j && !opts.yes && has_terminal() && {
                    use std::io::Write;
                    print!("Re-register this device? [Y/n] ");
                    let _ = std::io::stdout().flush();
                    !crate::util::prompt_line("").eq_ignore_ascii_case("n")
                };
                if re || opts.yes {
                    let _ = api.recover_identity().await;
                }
            }
            Err(_) => {}
        }
    }

    let api_base = api_base(&config);
    let dashboard_url = format!("{api_base}/dashboard");
    let claim_code = config.claim_code().unwrap_or_else(|| "(unknown)".into());
    let claim_url = if claimed {
        dashboard_url.clone()
    } else {
        config
            .claim_url()
            .unwrap_or_else(|| format!("{api_base}/device/{claim_code}"))
    };
    let agent_url = format!("{api_base}/device/{claim_code}/agent");

    emit(
        j,
        "registered",
        json!({
            "device_uuid": config.device_uuid(),
            "device_id": config.device_id(),
            "claimed": claimed,
            "claim_code": if claimed { Value::Null } else { json!(claim_code) },
            "claim_url": claim_url,
            "agent_url": agent_url,
        }),
    );

    // ── 3. Summariser mode — the consent gate (§3.3) ────────────────────
    step(
        j,
        "Choosing where sessions get summarised (redaction stays on your machine)",
    );
    let non_interactive = opts.json || opts.yes || !has_terminal();
    if opts.mode.is_none() && fresh_install && non_interactive {
        // Consent must be explicit at least once (§3.3).
        eprintln!(
            "modelstat: a summariser mode is required on a fresh non-interactive install.\n\
             Re-run with --mode <cloud|local|self-hosted>:\n\
             \x20 cloud        redacted turns are summarised on modelstat's servers (nothing extra installs)\n\
             \x20 local        (beta) a bundled model summarises on this machine (~2.7 GB download, ~4 GB RAM)\n\
             \x20 self-hosted  redacted excerpts go to your org's own summariser (needs --url)\n\
             Redaction always runs on-device first, in every mode."
        );
        return ExitCode::FAILURE;
    }
    let interactive_mode = !opts.yes && !opts.json && has_terminal();
    let mode = match crate::cmd_mode::resolve_and_persist_mode(
        &config,
        opts.mode.as_deref(),
        opts.url.as_deref(),
        interactive_mode,
    )
    .await
    {
        Ok(m) => m,
        Err(e) => {
            // The silent cloud fallback is removed (§23) — surface + fail.
            warn_line(j, &format!("mode selection failed: {e}"));
            eprintln!("modelstat: {e}");
            return ExitCode::FAILURE;
        }
    };
    if config.summarizer_mode_is_env_overridden() {
        warn_line(
            j,
            &format!(
                "MODELSTAT_SUMMARIZER_MODE is set — using \"{}\"",
                config.summarizer_mode()
            ),
        );
    }
    let mut mode_fields = Map::new();
    mode_fields.insert("mode".into(), json!(mode));
    if mode == "self-hosted" {
        mode_fields.insert("url".into(), json!(config.self_hosted_url()));
    }
    emit(j, "summarizer_mode", Value::Object(mode_fields));

    // ── 4. Model + engine setup (local engine BEFORE the daemon, §5.4) ──
    let mut model_ready = false;
    if mode == "local" {
        step(j, "Preparing local summariser (downloads on first run)");
        // Drive the engine's model download + install the engine service so the
        // daemon's first preflight finds it up.
        crate::cmd_mode::pre_download_engine_model();
        match install_service(Component::Summarizer, scope) {
            Ok(svc) => {
                model_ready = true;
                ok_line(j, "local summariser engine installed");
                emit(j, "summariser_model_ready", json!({}));
                emit(
                    j,
                    "summarizer_service_installed",
                    json!({ "path": svc.path.display().to_string() }),
                );
            }
            Err(e) => {
                warn_line(
                    j,
                    &format!("couldn't install the local engine ({e}) — it lazy-loads later"),
                );
                emit(
                    j,
                    "summariser_model_failed",
                    json!({ "error": e.to_string() }),
                );
                emit(
                    j,
                    "summarizer_service_failed",
                    json!({ "error": e.to_string() }),
                );
            }
        }
    } else {
        // cloud / self-hosted: no local engine armed here.
        let _ = uninstall_service(Component::Summarizer, scope);
        emit(j, "summariser_model_skipped", json!({ "mode": mode }));
    }
    // Every mode: pre-warm the on-device NER redactor (fail-closed on cloud/
    // self-hosted, so this keeps the first scan full-quality).
    step(
        j,
        "Preparing the on-device redactor (downloads the ~250 MB PII model)",
    );
    if modelstat_daemon::engine::ensure_ner_model().await {
        ok_line(j, "on-device redactor ready");
        emit(j, "redactor_model_ready", json!({}));
    } else {
        warn_line(
            j,
            "redactor model not ready — the daemon finishes it on its first scan",
        );
        emit(j, "redactor_model_not_ready", json!({}));
    }

    // ── 5. Service install ──────────────────────────────────────────────
    step(
        j,
        "Installing/refreshing background service so the daemon survives reboots",
    );
    let mut service_ok = false;
    match install_service(Component::Daemon, scope) {
        Ok(svc) => {
            service_ok = true;
            ok_line(j, &format!("service installed: {}", svc.path.display()));
            emit(
                j,
                "service_installed",
                json!({ "path": svc.path.display().to_string(), "logs": svc.logs.display().to_string(), "summariser_ready": model_ready }),
            );
        }
        Err(e) => {
            warn_line(
                j,
                &format!("service install failed ({e}) — run `modelstat start` manually"),
            );
            emit(
                j,
                "service_install_failed",
                json!({ "error": e.to_string() }),
            );
        }
    }

    // ── 5b. macOS tray (PREBUILT — no on-device compile, §5.2) ──────────
    // AFTER the daemon service on purpose: the tray's supervisor adopts a live
    // launchd-managed daemon, so booting it into a world where that daemon
    // already runs avoids ever hitting its converge path during install.
    if cfg!(target_os = "macos") {
        step(j, "Installing menu-bar tray (macOS)");
        if tray::tray_status().0 {
            match tray::install_tray_autostart() {
                Ok(Some(path)) => {
                    ok_line(j, "menu-bar tray ready");
                    emit(
                        j,
                        "tray_installed",
                        json!({ "path": path.display().to_string() }),
                    );
                    emit(j, "tray_autostart_installed", json!({ "ok": true }));
                }
                Ok(None) => emit(j, "tray_autostart_installed", json!({ "ok": false })),
                Err(e) => {
                    warn_line(j, &format!("tray autostart install failed: {e}"));
                    emit(
                        j,
                        "tray_autostart_installed",
                        json!({ "ok": false, "error": e.to_string() }),
                    );
                }
            }
        } else {
            warn_line(j, "prebuilt tray not bundled in this build — skipping");
            emit(j, "tray_not_bundled", json!({}));
        }
    }

    // ── 6. Claude Code statusline ───────────────────────────────────────
    if !env_flag("MODELSTAT_NO_STATUSLINE") {
        step(
            j,
            "Enabling the Claude Code statusline (live tokens · $ · taxonomy)",
        );
        let exe = std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "modelstat".into());
        use modelstat_daemon::claude_settings::{install_statusline, InstallResult};
        match install_statusline(&exe) {
            InstallResult::Installed { preserved } => {
                ok_line(j, "statusline enabled");
                emit(j, "statusline_installed", json!({ "preserved": preserved }));
            }
            InstallResult::Already => emit(j, "statusline_already", json!({})),
            InstallResult::Error(message) => {
                warn_line(j, &format!("couldn't enable the statusline: {message}"));
                emit(j, "statusline_failed", json!({ "error": message }));
            }
        }
    }

    // ── 7. Local discovery (banner counts) ──────────────────────────────
    step(j, "Detecting installed AI tools and signed-in accounts");
    let discovered = discover_counts();
    if let Some((installs, identities)) = discovered {
        ok_line(
            j,
            &format!("{installs} installations, {identities} identities"),
        );
        emit(
            j,
            "discovered",
            json!({ "installations": installs, "identities": identities }),
        );
    } else {
        emit(j, "discovery_failed", json!({ "error": "discovery error" }));
    }

    // ── 8. MCP wiring (EMBEDDED — no npx, §5.8) ─────────────────────────
    let mut mcp_wired = false;
    if std::env::var_os("MODELSTAT_NO_WIRE").is_some() {
        emit(
            j,
            "mcp_wire_skipped",
            json!({ "reason": "MODELSTAT_NO_WIRE" }),
        );
    } else {
        step(j, "Wiring the modelstat MCP into your AI tools");
        use modelstat_mcp::wire::{run_wire, Plat, WireStatus};
        let exe = std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "modelstat".into());
        let home = modelstat_ingest::modelstat_home();
        let results = run_wire(&home, Plat::current(), &exe, true);
        let n = results
            .iter()
            .filter(|r| r.status == WireStatus::Configured)
            .count();
        mcp_wired = n > 0 || results.iter().any(|r| r.status == WireStatus::Already);
        if mcp_wired {
            ok_line(j, "MCP wired into your AI tools");
            emit(j, "mcp_wired", json!({}));
        } else {
            emit(
                j,
                "mcp_wire_failed",
                json!({ "error": "no tools configured" }),
            );
        }
    }

    // ── 9. Banner + browser ─────────────────────────────────────────────
    if !j {
        print_banner(&BannerInfo {
            service_ok,
            discovered,
            claimed,
            claim_url: &claim_url,
            agent_url: &agent_url,
            mcp_wired,
        });
    }
    if !opts.no_browser {
        let opened = open_browser(&claim_url);
        emit(j, "browser_open_attempted", json!({ "opened": opened }));
    }
    emit(
        j,
        "done",
        json!({ "claim_url": claim_url, "agent_url": agent_url, "mcp_wired": mcp_wired }),
    );

    // Service-install failure in human mode → foreground fallback is the daemon's
    // job (`modelstat start`); connect itself has finished onboarding.
    let _ = service_ok;
    ExitCode::SUCCESS
}

struct BannerInfo<'a> {
    service_ok: bool,
    discovered: Option<(usize, usize)>,
    claimed: bool,
    claim_url: &'a str,
    agent_url: &'a str,
    mcp_wired: bool,
}

/// The 60×`━` onboarding banner (§5.9), byte-parity with the TS `cmdConnect` tail.
fn print_banner(b: &BannerInfo) {
    let tray_installed = tray::tray_status().0;
    let line = "━".repeat(60);
    println!();
    println!("{line}");
    println!("  ✓ Device registered — streaming your AI usage to modelstat.");
    println!();
    let (col, word) = if b.service_ok {
        ("32", "installed")
    } else {
        ("33", "foreground")
    };
    println!("    service : \x1b[{col}m{word}\x1b[0m");
    if let Some((installs, identities)) = b.discovered {
        println!("    detected: \x1b[32m{installs} installs · {identities} accounts\x1b[0m");
    }
    if cfg!(target_os = "macos") {
        let (col, word) = if tray_installed {
            ("32", "menu-bar icon ready")
        } else {
            ("2", "not installed")
        };
        println!("    tray    : \x1b[{col}m{word}\x1b[0m");
    }
    println!();
    println!(
        "{}",
        if b.claimed {
            "  Open your dashboard:"
        } else {
            "  Open your dashboard (no sign-up needed):"
        }
    );
    println!("    \x1b[1;36m{}\x1b[0m", b.claim_url);
    println!();
    println!("  Live numbers from this terminal:");
    println!("    \x1b[2mmodelstat status\x1b[0m  # pairing, service + sessions · tokens · cost");
    println!("    \x1b[2mmodelstat jobs\x1b[0m    # pipeline queue + recent activity");
    println!(
        "    \x1b[2mmodelstat mode\x1b[0m    # where sessions summarise (cloud/local/self-hosted)"
    );
    // The installer just put the bin dir on PATH, but a shell reads its startup
    // file once — THIS shell still can't see it. Say so, rather than printing
    // three commands that would answer "command not found".
    if !path_env::on_path(&modelstat_service::bin_dir()) {
        match path_env::source_hint() {
            Some(env) => {
                println!("    \x1b[33m!\x1b[0m \x1b[2mthis shell doesn't know `modelstat` yet — open a new terminal, or run:\x1b[0m");
                println!("      \x1b[2msource {}\x1b[0m", env.display());
            }
            None => println!("    \x1b[33m!\x1b[0m \x1b[2mopen a new terminal for `modelstat` to be found\x1b[0m"),
        }
    }
    println!();
    println!("  Agent-friendly (for LLMs / MCPs):");
    println!("    \x1b[2m{}\x1b[0m", b.agent_url);
    if b.mcp_wired {
        println!("    \x1b[32m✓\x1b[0m \x1b[2mMCP wired into your AI tools — ask them about your spend directly\x1b[0m");
    }
    if !b.claimed {
        println!();
        println!("  Claim this device so it keeps analyzing past the free tier:");
        println!("    \x1b[2m{}/claim\x1b[0m", b.claim_url);
    }
    println!("{line}");
    println!();
}

/// Best-effort local discovery counts `(installations, identities)`.
fn discover_counts() -> Option<(usize, usize)> {
    use modelstat_parsers::discovery::{discover, DiscoveryOptions};
    let out = discover(&DiscoveryOptions::default());
    Some((out.installations.len(), out.identities.len()))
}

/// `MODELSTAT_NO_STATUSLINE` truthiness (`1|true|yes`) — TS `envFlag`.
fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_connect_opts_reads_flags_and_values() {
        let o = parse_connect_opts(&args(&[
            "--json",
            "--no-browser",
            "-y",
            "--mode",
            "self-hosted",
            "--url",
            "http://x:4321",
        ]));
        assert!(o.json && o.no_browser && o.yes);
        assert!(!o.fresh && !o.system);
        assert_eq!(o.mode.as_deref(), Some("self-hosted"));
        assert_eq!(o.url.as_deref(), Some("http://x:4321"));
    }

    #[test]
    fn parse_connect_opts_defaults_all_off() {
        let o = parse_connect_opts(&args(&[]));
        assert!(!o.json && !o.no_browser && !o.fresh && !o.yes && !o.system);
        assert!(o.mode.is_none() && o.url.is_none());
    }

    #[test]
    fn parse_connect_opts_accepts_equals_form_and_long_yes() {
        let o = parse_connect_opts(&args(&["--mode=local", "--yes"]));
        assert_eq!(o.mode.as_deref(), Some("local"));
        assert!(o.yes);
    }

    // Locks the onboarding stream's wire contract (schema v1): the tray + harness
    // parse exactly `{v:1, ts, event, …fields}`.
    #[test]
    fn connect_line_wraps_event_in_schema_v1_envelope() {
        let line = connect_line(
            "registered",
            1_700_000_000_000,
            json!({ "device_id": "dev_x", "claimed": true }),
        );
        assert_eq!(line["v"], json!(1));
        assert_eq!(line["ts"], json!(1_700_000_000_000u64));
        assert_eq!(line["event"], json!("registered"));
        assert_eq!(line["device_id"], json!("dev_x"));
        assert_eq!(line["claimed"], json!(true));
    }

    #[test]
    fn connect_line_tolerates_non_object_fields() {
        // A null/non-object `fields` payload contributes no keys — still valid.
        let line = connect_line("tray_not_bundled", 1, json!(null));
        assert_eq!(line["event"], json!("tray_not_bundled"));
        assert_eq!(line.as_object().unwrap().len(), 3); // v, ts, event only
    }
}
