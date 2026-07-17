//! `mode [cloud|local|self-hosted] [--url <URL>] [--json]` (feature §9, §3.3).
//! Show or change where sessions get summarized; interactive set is the consent
//! gate. Port of `cmdMode`/`resolveAndPersistMode`.
//!
//! §9/§22 deltas from the TS daemon (spec is authoritative): self-hosted is now
//! "your org's own summarizer engine at one URL" — the BYO endpoint is dropped,
//! so there is NO `--model` / model id (the legacy `selfHostedModel` is
//! read-tolerated, ignored, dropped on write), the env override is
//! `MODELSTAT_SUMMARIZER_URL` (not `MODELSTAT_LLM_*`), and a best-effort
//! `/healthz` probe warns on unreachable/version skew but still persists (§10.4).

use std::process::ExitCode;
use std::sync::Arc;

use modelstat_ingest::state::{parse_summarizer_mode, set_self_hosted_url, set_summarizer_mode};
use modelstat_ingest::Config;
use modelstat_service::{install_service, stop_service, uninstall_service, Component, Scope};
use serde_json::{json, Map, Value};

use crate::util::{stdin_is_tty, text_prompt};

/// Plain-language copy for each mode (spec §3.3). Self-hosted is rewritten from
/// the TS "your own AI endpoint (URL + model)" to "your org's own summarizer" —
/// the BYO endpoint is gone (§22).
fn mode_info(mode: &str) -> (&'static str, &'static str, &'static str) {
    match mode {
        "cloud" => (
            "Cloud (default) — modelstat's servers summarise",
            "no local model; negligible RAM, CPU and battery on this machine",
            "your cleaned, redacted turns are uploaded and summarised server-side",
        ),
        "local" => (
            "Local — a bundled model summarises on THIS machine",
            "⚠ downloads a ~2.7 GB model (Qwen3.5-4B) and uses ~4 GB RAM plus extra battery/CPU while summarising",
            "only a ≤240-char abstract is uploaded; the raw turns never leave this machine",
        ),
        _ => (
            "Self-hosted — your org's own summariser engine",
            "no local model here; summarisation runs on the engine URL you provide",
            "only the abstract reaches modelstat; the cleaned excerpts go to your org's engine",
        ),
    }
}

/// The interactive consent-gate menu (§3.3). Returns the chosen mode string.
fn prompt_for_mode(current: &str) -> String {
    let opt = |n: &str, m: &str| {
        let (label, resource, privacy) = mode_info(m);
        format!("  {n}) {label}\n       resource: {resource}\n       privacy:  {privacy}\n")
    };
    print!(
        "\nHow should modelstat summarise your sessions?\n\
         Redaction (secrets + names/PII) ALWAYS runs on your machine first — only the\n\
         summarisation LOCATION below changes.\n\n{}{}{}\n",
        opt("1", "cloud"),
        opt("2", "local"),
        opt("3", "self-hosted"),
    );
    let raw = text_prompt(&format!("Choose 1-3 or a name [{current}]: "), current);
    match raw.trim() {
        "1" => "cloud".into(),
        "2" => "local".into(),
        "3" => "self-hosted".into(),
        other => parse_summarizer_mode(Some(other))
            .unwrap_or(current)
            .to_string(),
    }
}

/// http(s)-only URL validation (port of `validateSummarizerUrl`). Returns the
/// error message on failure so the caller can surface `couldn't set mode: …`.
fn validate_summarizer_url(url: &str) -> Result<(), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("summariser URL is not a valid URL: \"{url}\"."))?;
    match scheme {
        "http" | "https" if !rest.is_empty() => Ok(()),
        "http" | "https" => Err(format!("summariser URL is not a valid URL: \"{url}\".")),
        other => Err(format!(
            "summariser URL must use http(s): \"{url}\" (got \"{other}:\")."
        )),
    }
}

/// Resolve the requested mode + persist it (state.json). `requested` is the flag
/// or positional; `url_flag` is `--url`. Returns the resolved mode or an error
/// message. Port of `resolveAndPersistMode`. Reused by `connect` (step 3).
pub(crate) async fn resolve_and_persist_mode(
    config: &Config,
    requested: Option<&str>,
    url_flag: Option<&str>,
    interactive: bool,
) -> Result<String, String> {
    let mode = match requested {
        Some(r) if !r.is_empty() => parse_summarizer_mode(Some(r))
            .ok_or_else(|| {
                format!("unknown mode \"{r}\" — expected cloud, local, or self-hosted")
            })?
            .to_string(),
        _ if interactive => prompt_for_mode(&config.summarizer_mode()),
        // Non-interactive with nothing requested: keep the existing choice.
        _ => config.summarizer_mode(),
    };

    if mode == "self-hosted" {
        // URL: flag → MODELSTAT_SUMMARIZER_URL env → interactive prompt.
        let mut url = url_flag
            .map(str::to_string)
            .or_else(|| {
                std::env::var("MODELSTAT_SUMMARIZER_URL")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_default();
        if url.is_empty() && interactive {
            url = text_prompt(
                "  Self-hosted summariser engine URL (e.g. http://llm.acme.internal:4321): ",
                "",
            );
        }
        if url.is_empty() {
            return Err("self-hosted mode needs a summariser URL (--url <URL>)".into());
        }
        validate_summarizer_url(&url)?;
        // Best-effort reachability + skew probe — warns but still persists (§10.4).
        probe_self_hosted(&url).await;
        set_self_hosted_url(&url).map_err(|e| e.to_string())?;
    } else {
        // Leaving self-hosted — clear the stored URL so a stale one can't resurface.
        set_self_hosted_url("").map_err(|e| e.to_string())?;
    }
    set_summarizer_mode(&mode).map_err(|e| e.to_string())?;
    Ok(mode)
}

/// Best-effort `GET /healthz` on a self-hosted engine: warns loudly on
/// unreachable / protocol skew but never blocks the mode set (§10.4).
async fn probe_self_hosted(url: &str) {
    use modelstat_sumclient::SummarizerClient;
    let client = SummarizerClient::new(url.to_string());
    match client.healthz().await {
        Ok(h) if h.protocol != 1 => eprintln!(
            "  ⚠ engine at {url} speaks protocol {} (this collector expects 1) — summaries may fail until it's upgraded",
            h.protocol
        ),
        Ok(_) => {}
        Err(_) => eprintln!(
            "  ⚠ couldn't reach the summariser at {url} yet — saved anyway; summaries hold + retry until it's up"
        ),
    }
}

/// Arm (local) or disarm (cloud/self-hosted) the local engine service, keeping
/// model files on disk. Best-effort — a service hiccup is a warning, not a stall.
async fn reconcile_local_engine(mode: &str) {
    if mode == "local" {
        // Pre-warm the Qwen model with progress via the engine's own setup
        // (best-effort; the engine also lazy-downloads on first serve, §11), then
        // arm the loopback engine service.
        pre_download_engine_model();
        match install_service(Component::Summarizer, Scope::User) {
            Ok(_) => println!("✓ local summariser engine service armed"),
            Err(e) => eprintln!("  couldn't arm the local engine service ({e}) — run `modelstat` to retry"),
        }
    } else {
        // cloud / self-hosted: stop + remove the local engine (model files kept).
        let _ = stop_service(Component::Summarizer, Scope::User);
        let _ = uninstall_service(Component::Summarizer, Scope::User);
    }
}

/// Drive the Qwen download with progress by shelling the engine binary's `setup`
/// (which writes `summarizer.json` + downloads the model). Best-effort: a missing
/// engine binary just means the service lazy-downloads on first use (§11). Shared
/// with `connect` (step 4).
pub(crate) fn pre_download_engine_model() {
    let engine = engine_binary_path();
    println!("preparing local summariser model (downloads on first use)…");
    // `--loopback`: config + model only, no prompts, no service — the collector
    // arms the loopback engine service itself (§10.3 "driven inline").
    let _ = std::process::Command::new(engine).args(["setup", "--loopback"]).status();
}

/// The engine binary next to this collector (install layout), else on PATH.
fn engine_binary_path() -> std::path::PathBuf {
    let name = if cfg!(windows) {
        "modelstat-summarizer.exe"
    } else {
        "modelstat-summarizer"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sib = dir.join(name);
            if sib.exists() {
                return sib;
            }
        }
    }
    std::path::PathBuf::from(name)
}

pub async fn cmd_mode(config: Arc<Config>, args: &[String]) -> ExitCode {
    let as_json = args.iter().any(|a| a == "--json");
    let flag_val = |name: &str| flag_value(args, name);
    // A bare positional (not a --flag) is the mode; --mode is also accepted.
    let positional = args.iter().find(|a| !a.starts_with('-')).cloned();
    let requested = flag_val("--mode").or(positional);

    // ── show mode (no argument) ─────────────────────────────────────────
    if requested.is_none() {
        let mode = config.summarizer_mode();
        if as_json {
            let mut m = Map::new();
            m.insert("mode".into(), json!(mode));
            if mode == "self-hosted" {
                m.insert("url".into(), json!(config.self_hosted_url()));
            }
            m.insert(
                "env_override".into(),
                json!(config.summarizer_mode_is_env_overridden()),
            );
            println!("{}", serde_json::to_string(&Value::Object(m)).unwrap());
            return ExitCode::SUCCESS;
        }
        let (label, resource, privacy) = mode_info(&mode);
        println!("summariser mode: {mode}");
        println!("  {label}");
        println!("  resource: {resource}");
        println!("  privacy:  {privacy}");
        if mode == "self-hosted" {
            let url = config.self_hosted_url();
            println!("  endpoint: {}", if url.is_empty() { "(unset)" } else { &url });
        }
        if config.summarizer_mode_is_env_overridden() {
            println!("  note: MODELSTAT_SUMMARIZER_MODE is set and overrides the stored value.");
        }
        println!("change it: modelstat mode <cloud|local|self-hosted> [--url <URL>]");
        return ExitCode::SUCCESS;
    }

    // ── set mode ────────────────────────────────────────────────────────
    let interactive = !as_json && stdin_is_tty();
    let mode = match resolve_and_persist_mode(&config, requested.as_deref(), flag_val("--url").as_deref(), interactive).await {
        Ok(m) => m,
        Err(e) => {
            eprintln!("couldn't set mode: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("✓ summariser mode set to {mode}");
    if config.summarizer_mode_is_env_overridden() {
        eprintln!(
            "⚠ MODELSTAT_SUMMARIZER_MODE is set — the daemon will use \"{}\" until you unset it",
            config.summarizer_mode()
        );
    }

    // Arm/disarm the local engine for the new mode, then pull the NER redactor
    // (fail-closed on cloud/self-hosted) so the next scan is full-quality.
    reconcile_local_engine(&mode).await;
    println!("preparing the on-device redactor (downloads on first use)…");
    if modelstat_daemon::engine::ensure_ner_model().await {
        println!("✓ on-device redactor ready");
    } else {
        eprintln!("redactor model not ready yet — the daemon finishes it on its first scan");
    }

    // Bounce the daemon service so the running daemon reloads the new mode — only
    // when there's an installed (paired) daemon to refresh.
    if config.bearer().is_some() {
        match install_service(Component::Daemon, Scope::User) {
            Ok(svc) => println!("✓ background service refreshed ({})", svc.path.display()),
            Err(e) => eprintln!(
                "couldn't refresh the service ({e}) — restart it by re-running `modelstat`"
            ),
        }
    } else {
        println!("run `modelstat` to install the background service with this mode.");
    }
    ExitCode::SUCCESS
}

/// `--flag v` / `--flag=v` reader (repeat-free; first wins). Shared shape with
/// the other commands' parsing.
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
