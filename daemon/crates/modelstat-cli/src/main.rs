//! `modelstat` — the collector binary entry point.
//!
//! M1 (plan §5) wires the identity/device commands: `self-register` (with the
//! prod/CI register guard), `await-claim`, `token`, and `paths`. The rest of the
//! surface (connect, start, mcp, statusline, status/jobs/mode/sync/…) lands
//! across M4–M6. Command behaviour, flags, outputs, and exit codes are preserved
//! from the TS `apps/daemon/src/cli.ts` unless feature §5/§23 says otherwise.

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;

use modelstat_ingest::{
    build_fingerprint, identity_path, intended_device_uuid, logs_dir, machine_key_source,
    state_path, Config, DeviceApi, DeviceMeError, FreshIdentity,
};
use serde::Serialize;

mod cmd_connect;
mod cmd_mode;
mod cmd_status;
mod util;

/// Compile-time version string, `daemon-<semver>` (feature §5).
const VERSION: &str = concat!("daemon-", env!("CARGO_PKG_VERSION"));

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") | Some("-v") | Some("version") => {
            println!("{VERSION}");
            ExitCode::SUCCESS
        }
        // Onboarding (feature §5). The default command (no subcommand), `connect`,
        // and `reinstall` are identical; it registers/reuses identity, installs
        // services, wires the MCP, and prints the banner.
        None | Some("connect") | Some("reinstall") => {
            let start = if args.is_empty() { 0 } else { 1 };
            let opts = cmd_connect::parse_connect_opts(&args[start..]);
            block_on(|api| async move { cmd_connect::cmd_connect(&api, opts).await })
        }
        Some("self-register") => block_on(|api| async move { cmd_self_register(&api).await }),
        Some("await-claim") => block_on(|api| async move { cmd_await_claim(&api).await }),
        Some("token") => cmd_token(&args[1..]),
        Some("paths") => cmd_paths(&args[1..]),
        // Pairing + service + usage snapshot; `--json` is the tray's frozen poll
        // API. Strictly side-effect-free (§5) — hence the device probe rides the
        // shared DeviceApi but never recovers identity.
        Some("status") => block_on(|api| async move { cmd_status::cmd_status(&api, &args[1..]).await }),
        Some("jobs") => block_on(|api| async move { cmd_status::cmd_jobs(&api, &args[1..]).await }),
        // Show/change the summarizer mode (§9); interactive set = the consent gate.
        Some("mode") => block_on_config(|config| async move { cmd_mode::cmd_mode(config, &args[1..]).await }),
        // The long-running collector daemon (feature §5). `start` and `run` are
        // aliases; `--force` replaces a live owner (see the singleton lock).
        Some("start") | Some("run") => cmd_run(&args[1..]),
        // Claude Code statusline (§14) — reads stdin, prints one line, never
        // blocks/throws. No async runtime + no network: a pure cache read.
        Some("statusline") => {
            modelstat_daemon::statusline::run_statusline();
            ExitCode::SUCCESS
        }
        // Embedded MCP (§12). `mcp wire [--heal]` configures clients; bare `mcp`
        // runs the stdio bridge.
        Some("mcp") => cmd_mcp(&args[1..]),
        // The tray's adopt/spawn/replace decision (§15) — read-only JSON.
        Some("_daemon-health") => cmd_daemon_health(),
        // Install/refresh the managed daemon service + reconcile the tray (§16/§15).
        Some("_install-service") => cmd_install_service(),
        _ => {
            println!("{VERSION}");
            println!("the collector CLI is implemented across milestones M1–M6");
            ExitCode::SUCCESS
        }
    }
}

/// `modelstat start` / `modelstat run` — the collector daemon. Builds a
/// multi-thread runtime (the daemon fans heartbeat + receiver + scan + drain +
/// reconcile across worker threads, and a scan's synchronous git subprocess reads
/// must not block liveness) and hands control to the main loop. The process runs
/// until a signal or a lost lock race; its exit code (0 / 130 / 143 / 1) comes
/// straight from `run`.
fn cmd_run(args: &[String]) -> ExitCode {
    let force = args.iter().any(|a| a == "--force");
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("modelstat: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let config = Arc::new(Config::load(VERSION));
    rt.block_on(modelstat_daemon::run::run(config, force))
}

/// `modelstat mcp [wire [--heal]]` (§12). Bare `mcp` runs the stdio JSON-RPC
/// bridge (stdout = protocol only; logs to stderr; fatal → stderr + exit 1);
/// `mcp wire` configures the MCP into detected clients.
fn cmd_mcp(args: &[String]) -> ExitCode {
    if args.first().map(String::as_str) == Some("wire") {
        return cmd_mcp_wire(&args[1..]);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("modelstat-mcp: fatal: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(modelstat_mcp::runtime::run_bridge(VERSION));
    ExitCode::SUCCESS
}

/// `modelstat mcp wire [--heal]` (§12) — configure the modelstat MCP server into
/// every detected AI tool. `--heal` (run by the daemon on startup) touches only
/// clients we haven't wired before. Always exits 0. Logs to stderr prefixed
/// `modelstat-mcp: ` (stdout is reserved for the MCP protocol).
fn cmd_mcp_wire(args: &[String]) -> ExitCode {
    use modelstat_mcp::wire::{heal_wire, run_wire, wired_state_path, Plat, WireStatus};
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "modelstat".to_string());
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    if args.iter().any(|a| a == "--heal") {
        let r = heal_wire(&home, Plat::current(), &exe, true, &wired_state_path());
        if !r.configured.is_empty() {
            eprintln!("modelstat-mcp: wired {}", r.configured.join(", "));
        }
        return ExitCode::SUCCESS;
    }
    let results = run_wire(&home, Plat::current(), &exe, true);
    eprintln!("modelstat MCP — wiring your AI tools:");
    for r in &results {
        eprintln!("  {} {} — {}", r.status.mark(), r.name, r.status.label());
    }
    let n = results
        .iter()
        .filter(|r| r.status == WireStatus::Configured)
        .count();
    if n > 0 {
        eprintln!(
            "Configured {n} tool{}. Restart any open tool to load the modelstat MCP.",
            if n == 1 { "" } else { "s" }
        );
    } else {
        eprintln!("Nothing new to configure (already set up, or no supported tools detected).");
    }
    ExitCode::SUCCESS
}

/// `modelstat _daemon-health` — the tray's read-only adopt/spawn/replace decision
/// (§15). Never throws: `daemon_health` returns a valid decision even with no
/// lock (→ spawn).
fn cmd_daemon_health() -> ExitCode {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let health = modelstat_daemon::supervise::daemon_health(now_ms, Some(VERSION));
    let json = modelstat_daemon::supervise::daemon_health_json(&health);
    println!("{}", serde_json::to_string(&json).unwrap());
    ExitCode::SUCCESS
}

/// `modelstat _install-service` — install/refresh the managed daemon service +
/// reconcile the macOS tray agent, one idempotent step (used by connect +
/// self-update). The summarizer engine service + statusline are installed by
/// `connect` when the mode calls for them (M6).
fn cmd_install_service() -> ExitCode {
    use modelstat_service::{install_service, tray, Component, Scope};
    match install_service(Component::Daemon, Scope::User) {
        Ok(r) => {
            println!("\x1b[32m✓\x1b[0m daemon service installed: {}", r.path.display());
            println!("  logs: {}", r.logs.display());
        }
        Err(e) => {
            eprintln!("modelstat: service install failed: {e}");
            return ExitCode::FAILURE;
        }
    }
    if tray::ensure_tray_installed() {
        println!("\x1b[32m✓\x1b[0m tray agent reconciled");
    }
    ExitCode::SUCCESS
}

/// Build a runtime, construct the shared [`Config`] + [`DeviceApi`], and run one
/// async command. The device flow is a handful of HTTP calls, so a single-thread
/// runtime is plenty.
fn block_on<F, Fut>(f: F) -> ExitCode
where
    F: FnOnce(Arc<DeviceApi>) -> Fut,
    Fut: std::future::Future<Output = ExitCode>,
{
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("modelstat: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let config = Arc::new(Config::load(VERSION));
    let api = Arc::new(DeviceApi::new(config));
    rt.block_on(f(api))
}

/// Like [`block_on`] but hands the command the shared [`Config`] directly (for
/// commands that reconcile services / download models but don't drive the device
/// register/recover flow). A current-thread runtime is plenty — these commands
/// run one thing at a time.
fn block_on_config<F, Fut>(f: F) -> ExitCode
where
    F: FnOnce(Arc<Config>) -> Fut,
    Fut: std::future::Future<Output = ExitCode>,
{
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("modelstat: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(f(Arc::new(Config::load(VERSION))))
}

// ── env / TTY helpers (TS cli.ts) ───────────────────────────────────────

/// Non-interactive when `CI` is set to any non-empty value (JS truthiness), or
/// when NONE of stdin/stdout/stderr is a TTY. Port of TS `isNonInteractive`.
fn is_non_interactive() -> bool {
    if std::env::var("CI").map(|v| !v.is_empty()).unwrap_or(false) {
        return true;
    }
    !(std::io::stdin().is_terminal()
        || std::io::stdout().is_terminal()
        || std::io::stderr().is_terminal())
}

/// True when an env var is set to a truthy value (`1`/`true`/`yes`). Port of TS
/// `envFlag`.
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

/// Explicit "yes, really register against prod headlessly" opt-in.
fn prod_register_opt_in() -> bool {
    env_flag("MODELSTAT_ALLOW_PROD_REGISTER")
}

// ── self-register (feature §5, §4 CI guard) ─────────────────────────────

async fn cmd_self_register(api: &DeviceApi) -> ExitCode {
    let config = api.config().clone();

    // Device UUID resolution: keep a UUID already in identity.json (never churn
    // an existing enrollment), else derive it deterministically from the machine
    // key — so a fresh/wiped install lands on the SAME UUID and dedupes back to
    // the existing server row instead of minting a duplicate.
    let stored_uuid = config.device_uuid();
    let derived = stored_uuid.is_none();
    let device_uuid = stored_uuid.unwrap_or_else(intended_device_uuid);

    // CI guard (feature §4): never silently create a NEW device on PRODUCTION
    // from a non-interactive/CI environment (no claim is possible there anyway,
    // and ephemeral runners were piling up unclaimed rows + paging ops). An
    // already-enrolled device re-registering is untouched.
    if derived && config.is_prod_default_api() && is_non_interactive() && !prod_register_opt_in() {
        eprint!(
            "modelstat: refusing to self-register a new device against production from a\n\
             non-interactive/CI environment (no claim is possible here anyway). Either:\n\
             \x20 • point at your own backend:  DAEMON_API_URL=https://your-host   (CI/e2e)\n\
             \x20 • explicitly opt in:          MODELSTAT_ALLOW_PROD_REGISTER=1\n\
             \x20 • or run it interactively:    modelstat\n"
        );
        return ExitCode::from(2);
    }

    // ONE fingerprint, shared with the heartbeat (feature §4). `machine_id` is
    // the server's dedupe anchor.
    let fingerprint = build_fingerprint(config.version());

    if derived {
        let short = &device_uuid[..device_uuid.len().min(8)];
        println!(
            "  \x1b[2mdevice id derived from machine key ({}): {short}…\x1b[0m",
            machine_key_source()
        );
    }
    println!("  \x1b[2m→ POST {}/v1/tokens\x1b[0m", config.api_url());

    let res = match api.self_register(device_uuid, fingerprint).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // Seed the canonical identity file atomically — a single write so it is never
    // half-populated if the process dies mid-sequence. The `ds_live_…` secret is
    // stored verbatim.
    if let Err(e) = config.save_fresh_identity(FreshIdentity {
        device_uuid: res.device_uuid.clone(),
        device_id: res.device_id.clone(),
        bearer_token: res.device_secret.clone(),
        claim_code: res.claim_code.clone(),
        claim_url: res.claim_url.clone(),
    }) {
        eprintln!("failed to save identity: {e}");
        return ExitCode::FAILURE;
    }

    let verb = if res.re_registered.unwrap_or(false) {
        "re-registered"
    } else {
        "registered"
    };
    println!("  \x1b[32m✓\x1b[0m {verb}  device_id={}", res.device_id);
    println!(
        "  \x1b[32m✓\x1b[0m secret      {}…  (hashed on server, never re-sent)",
        res.secret_prefix
    );
    if res.status == "claimed" {
        // Already attached — no live claim handle; point at the dashboard rather
        // than dangle a dead claim link.
        let base = config.api_url();
        let base = base.strip_suffix('/').unwrap_or(&base);
        let by = res
            .user_id
            .as_deref()
            .map(|u| format!(" by user_id={u}"))
            .unwrap_or_default();
        println!("  \x1b[32m✓\x1b[0m already claimed{by} — open {base}/dashboard");
    } else if let Some(cc) = res.claim_code.as_deref() {
        println!("  \x1b[32m✓\x1b[0m claim code  {cc}");
    }
    ExitCode::SUCCESS
}

// ── await-claim (feature §5) ────────────────────────────────────────────

async fn cmd_await_claim(api: &DeviceApi) -> ExitCode {
    let config = api.config().clone();
    let Some(secret0) = config.bearer() else {
        eprintln!("not registered — run `modelstat self-register` first");
        return ExitCode::FAILURE;
    };
    let url = config
        .claim_url()
        .unwrap_or_else(|| "(visit your dashboard)".to_string());
    println!("waiting for human to claim this device:\n    {url}\n");
    loop {
        // Always read the CURRENT bearer — recover_identity below may rotate it.
        let secret = config.bearer().unwrap_or_else(|| secret0.clone());
        match api.fetch_device_me(&secret).await {
            Ok(me) => {
                if me.status == "claimed" {
                    println!(
                        "✓ claimed by user_id={}",
                        me.user_id.as_deref().unwrap_or("null")
                    );
                    return ExitCode::SUCCESS;
                }
                print!(".");
                let _ = std::io::stdout().flush();
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(DeviceMeError::Unauthorized) => {
                // Server no longer accepts our bearer (revoked / row deleted).
                // Recover by machine-stable re-register (self-rate-limited), then
                // poll the fresh bearer — never busy-loop the dead secret.
                let recovered = api.recover_identity().await;
                eprintln!(
                    "{}",
                    if recovered {
                        "re-registered after the server rejected our credentials — resuming claim wait"
                    } else {
                        "couldn't re-register yet (server rejecting registration) — backing off"
                    }
                );
                let secs = if recovered { 2 } else { 5 };
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            }
            Err(DeviceMeError::Other(msg)) => {
                eprintln!("poll failed: {msg}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

// ── token (feature §5) ──────────────────────────────────────────────────

#[derive(Serialize)]
struct TokenJson {
    token: String,
    api: String,
}

fn cmd_token(args: &[String]) -> ExitCode {
    let config = Config::load(VERSION);
    let Some(bearer) = config.bearer() else {
        eprintln!("not paired — run `modelstat` first");
        return ExitCode::FAILURE;
    };
    if args.iter().any(|a| a == "--json") {
        let out = TokenJson {
            token: bearer,
            api: config.api_url(),
        };
        println!("{}", serde_json::to_string(&out).unwrap());
        return ExitCode::SUCCESS;
    }
    // Bare token on stdout so `$(modelstat token)` substitutes cleanly.
    println!("{bearer}");
    if std::io::stderr().is_terminal() {
        eprintln!("(device token — treat it like a password; rotate via the dashboard if leaked)");
    }
    ExitCode::SUCCESS
}

// ── paths (feature §5) ──────────────────────────────────────────────────

/// The `paths --json` contract (feature §5). A serde struct — NOT a `json!`
/// Value — so the key ORDER is preserved (`serde_json::Map` sorts by default).
/// Consumed by the standalone MCP package.
#[derive(Serialize)]
struct PathsJson {
    state: String,
    identity: String,
    logs: String,
    api: String,
    paired: bool,
    device_id: String,
    device_uuid: String,
    intended_uuid: String,
    machine_key_source: &'static str,
}

fn cmd_paths(args: &[String]) -> ExitCode {
    let config = Config::load(VERSION);
    // intended_uuid is what THIS machine would self-register as today; it should
    // equal device_uuid for a healthy enrollment (a mismatch is still fine —
    // machine_id dedupe covers it server-side).
    let data = PathsJson {
        state: state_path().display().to_string(),
        identity: identity_path().display().to_string(),
        logs: logs_dir().display().to_string(),
        api: config.api_url(),
        paired: config.bearer().is_some() && config.device_id().is_some(),
        device_id: config.device_id().unwrap_or_else(|| "(none)".to_string()),
        device_uuid: config.device_uuid().unwrap_or_else(|| "(none)".to_string()),
        intended_uuid: intended_device_uuid(),
        machine_key_source: machine_key_source(),
    };
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string(&data).unwrap());
        return ExitCode::SUCCESS;
    }
    // Human form: `key.padEnd(8) value`, in the same order (TS `Object.entries`).
    let rows: [(&str, String); 9] = [
        ("state", data.state),
        ("identity", data.identity),
        ("logs", data.logs),
        ("api", data.api),
        ("paired", data.paired.to_string()),
        ("device_id", data.device_id),
        ("device_uuid", data.device_uuid),
        ("intended_uuid", data.intended_uuid),
        ("machine_key_source", data.machine_key_source.to_string()),
    ];
    for (k, v) in rows {
        println!("{k:<8} {v}");
    }
    ExitCode::SUCCESS
}
