//! `modelstat redactor [local|cloud|self-hosted]` — where turns get SCRUBBED.
//!
//! Its own setting, separate from `modelstat mode` (where turns get SUMMARISED),
//! because they are separate questions with different answers: one setting could
//! not say "scrub here, summarise there".
//!
//! What never moves, in any mode: the layer-1 deterministic floor. Secrets,
//! emails, key-shaped blobs and home paths are scrubbed on this machine before
//! any byte leaves it. The modes only decide where the layer-2 PII model runs —
//! on this machine (`local`), on modelstat's servers (`cloud`, the default), or
//! on an endpoint the org operates (`self-hosted`). Remote modes are
//! fail-closed like the local one: an endpoint that cannot answer means the
//! flush HOLDS, never "ship it less redacted".

use std::process::ExitCode;

use modelstat_ingest::{state, Config};
use modelstat_service::{install_service, Component, Scope};

/// Plain-language copy per redactor mode: (title, what actually happens).
pub(crate) fn redactor_info(mode: &str) -> (&'static str, &'static str) {
    match mode {
        "local" => (
            "Local — this machine scrubs everything",
            "the PII model (~900 MB, one download) runs on-device; even \
             floor-scrubbed text never leaves until it is fully redacted",
        ),
        "cloud" => (
            "Cloud (default) — modelstat's servers run the PII model",
            "secrets, emails, keys and paths are still scrubbed on this machine \
             first (always); the floor-scrubbed text is then classified on \
             modelstat's servers, which return the spans and store nothing — \
             splicing happens here, and only fully-redacted turns are uploaded",
        ),
        _ => (
            "Self-hosted — your org's endpoint runs the PII model",
            "same contract as cloud, against an endpoint you operate \
             (`modelstat-redactor`, or core's docker sidecar); the floor still \
             runs on this machine first, and an unreachable endpoint holds \
             uploads rather than degrading",
        ),
    }
}

/// http(s)-only URL validation, the redactor twin of `validate_summarizer_url`.
fn validate_redactor_url(url: &str) -> Result<(), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("redactor URL is not a valid URL: \"{url}\"."))?;
    match scheme {
        "http" | "https" if !rest.is_empty() => Ok(()),
        "http" | "https" => Err(format!("redactor URL is not a valid URL: \"{url}\".")),
        other => Err(format!(
            "redactor URL must use http(s): \"{url}\" (got \"{other}:\")."
        )),
    }
}

/// Best-effort healthz probe — warns loudly, never blocks the setting (the
/// daemon holds + retries until the endpoint answers, and says so).
async fn probe_redactor(base: &str, bearer: Option<String>) {
    use modelstat_ingest::redactor_client::RemoteRedactor;
    match RemoteRedactor::new(base, bearer).healthz().await {
        Some(h) if h.protocol != modelstat_redact::remote::REDACT_PROTOCOL => eprintln!(
            "  ⚠ redactor at {base} speaks protocol {} (this daemon expects {}) — \
             uploads hold until one of them is upgraded",
            h.protocol,
            modelstat_redact::remote::REDACT_PROTOCOL
        ),
        Some(h) if !h.model_loaded => {
            eprintln!("  ⚠ redactor at {base} is still loading its model — uploads hold until it's ready")
        }
        Some(_) => {}
        None => eprintln!(
            "  ⚠ couldn't reach the redactor at {base} yet — saved anyway; uploads hold + retry until it's up"
        ),
    }
}

/// Interactive picker, the redactor twin of `prompt_for_mode`. Default = the
/// current stored choice (cloud on a fresh install — `DEFAULT_REDACTOR_MODE`).
fn prompt_for_redactor(current: &str) -> String {
    let opt = |n: &str, m: &str| {
        let (title, detail) = redactor_info(m);
        format!(
            "  {n}) {title}
       {detail}
"
        )
    };
    print!(
        "
Where should the PII model run? The secret floor (keys, emails, paths)
         ALWAYS runs on this machine first — this only places the second, model
         pass.

{}{}{}
",
        opt("1", "cloud"),
        opt("2", "local"),
        opt("3", "self-hosted"),
    );
    let raw = crate::util::text_prompt(&format!("Choose 1-3 or a name [{current}]: "), current);
    match raw.trim() {
        "1" => "cloud".into(),
        "2" => "local".into(),
        "3" => "self-hosted".into(),
        other => state::parse_redactor_mode(Some(other))
            .unwrap_or(current)
            .to_string(),
    }
}

/// Resolve + persist the redactor mode, the twin of
/// `cmd_mode::resolve_and_persist_mode`: explicit request wins, else the
/// interactive picker (default = current choice, cloud on a fresh install),
/// else keep the current choice. Self-hosted validates + probes its URL
/// before persisting; nothing persists on error.
pub(crate) async fn resolve_and_persist_redactor(
    config: &Config,
    requested: Option<&str>,
    url_flag: Option<&str>,
    interactive: bool,
) -> Result<String, String> {
    let current = config.redactor_mode();
    let mode = match requested {
        Some(r) if !r.is_empty() => state::parse_redactor_mode(Some(r))
            .ok_or_else(|| {
                format!(
                    "unknown redactor \"{r}\" — expected one of {}",
                    state::REDACTOR_MODES.join(", ")
                )
            })?
            .to_string(),
        _ if interactive => prompt_for_redactor(&current),
        _ => current,
    };
    if mode == "self-hosted" {
        let mut url = url_flag
            .map(str::to_string)
            .or_else(|| {
                std::env::var("MODELSTAT_REDACTOR_URL")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_default();
        if url.is_empty() && interactive {
            url = crate::util::text_prompt(
                "  Self-hosted redactor endpoint URL (e.g. http://redact.acme.internal:8090): ",
                "",
            );
        }
        if url.is_empty() {
            return Err("self-hosted redaction needs an endpoint (--redactor-url <URL>)".into());
        }
        validate_redactor_url(&url)?;
        probe_redactor(&url, None).await;
        state::set_redactor_url(&url).map_err(|e| e.to_string())?;
    } else {
        state::set_redactor_url("").map_err(|e| e.to_string())?;
    }
    if mode == "cloud" {
        probe_redactor(&config.api_url(), config.bearer()).await;
    }
    state::set_redactor_mode(&mode).map_err(|e| e.to_string())?;
    Ok(mode)
}

pub async fn cmd_redactor(config: &Config, args: &[String]) -> ExitCode {
    let positional = args.iter().find(|a| !a.starts_with('-')).cloned();
    let url_flag = args
        .iter()
        .position(|a| a == "--url")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let current = config.redactor_mode();

    // No argument: report where scrubbing runs, and the floor guarantee.
    let Some(requested) = positional.map(|s| s.trim().to_lowercase()) else {
        let (title, detail) = redactor_info(&current);
        println!("redactor: {current}");
        println!("  {title}");
        println!("  {detail}");
        if current == "self-hosted" {
            println!("  endpoint: {}", config.redactor_url());
        }
        if config.redactor_mode_is_env_overridden() {
            println!("  note: MODELSTAT_REDACTOR_MODE is set and overrides the stored choice");
        }
        println!("  the secret floor always runs on this machine, in every mode");
        println!("summariser: {}", config.summarizer_mode());
        println!("  change the summariser with `modelstat mode`");
        return ExitCode::SUCCESS;
    };

    let Some(mode) = state::parse_redactor_mode(Some(&requested)) else {
        eprintln!(
            "modelstat: unknown redactor `{requested}` — expected one of {}",
            state::REDACTOR_MODES.join(", ")
        );
        return ExitCode::from(2);
    };

    if mode == "self-hosted" {
        let url = url_flag
            .or_else(|| {
                std::env::var("MODELSTAT_REDACTOR_URL")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_default();
        if url.is_empty() {
            eprintln!("modelstat: self-hosted redaction needs an endpoint (--url <URL>)");
            return ExitCode::from(2);
        }
        if let Err(e) = validate_redactor_url(&url) {
            eprintln!("modelstat: {e}");
            return ExitCode::from(2);
        }
        probe_redactor(&url, None).await;
        if let Err(e) = state::set_redactor_url(&url) {
            eprintln!("modelstat: could not save the redactor URL: {e}");
            return ExitCode::FAILURE;
        }
    } else {
        // Leaving self-hosted — clear the stored URL so a stale one can't resurface.
        if let Err(e) = state::set_redactor_url("") {
            eprintln!("modelstat: could not clear the redactor URL: {e}");
            return ExitCode::FAILURE;
        }
    }
    if mode == "cloud" {
        probe_redactor(&config.api_url(), config.bearer()).await;
    }

    if let Err(e) = state::set_redactor_mode(mode) {
        eprintln!("modelstat: could not save the redactor setting: {e}");
        return ExitCode::FAILURE;
    }
    let (title, detail) = redactor_info(mode);
    println!("redactor: {mode}");
    println!("  {title}");
    println!("  {detail}");

    // Switching TO local needs the on-device model; fetch it now with progress
    // so the first scan doesn't start held.
    if mode == "local" {
        println!("preparing the on-device redactor (~900 MB, downloads once)…");
        if modelstat_daemon::engine::ensure_redactor_model().await {
            println!("✓ on-device redactor ready");
        } else {
            eprintln!("redactor model not ready — the daemon keeps retrying in the background");
        }
    }

    // Bounce the daemon so the running process rebuilds its redactor for the
    // new mode (it is resolved once at boot) — only when there's a paired
    // daemon to refresh, mirroring `modelstat mode`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_has_copy_and_every_copy_names_the_floor_or_the_model() {
        for m in state::REDACTOR_MODES {
            let (title, detail) = redactor_info(m);
            assert!(!title.is_empty() && !detail.is_empty(), "{m}");
        }
        // The remote modes must say what still happens on-device — that copy is
        // the consent surface, and "the floor runs first" is its load-bearing
        // sentence.
        for m in ["cloud", "self-hosted"] {
            let (_, detail) = redactor_info(m);
            assert!(
                detail.contains("floor") && detail.contains("this machine"),
                "{m} copy must state the on-device floor guarantee"
            );
        }
        let (_, cloud) = redactor_info("cloud");
        assert!(
            cloud.contains("store nothing"),
            "cloud copy must state the no-storage contract"
        );
    }

    #[test]
    fn both_defaults_are_cloud() {
        // The product pairing: both axes default to cloud; local is the
        // explicit privacy opt-out for each.
        assert_eq!(state::DEFAULT_REDACTOR_MODE, "cloud");
        assert_eq!(state::DEFAULT_SUMMARIZER_MODE, "cloud");
    }

    #[test]
    fn url_validation_is_http_s_only() {
        assert!(validate_redactor_url("https://redactor.acme.internal:8477").is_ok());
        assert!(validate_redactor_url("http://10.0.0.5:8477").is_ok());
        assert!(validate_redactor_url("ftp://x").is_err());
        assert!(validate_redactor_url("not-a-url").is_err());
        assert!(validate_redactor_url("http://").is_err());
    }
}
