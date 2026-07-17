//! `modelstat-summarizer` — the inference-engine binary (feature §10).
//!
//! Commands: `serve` (the protocol-v1 server), `setup` (writes `summarizer.json`
//! and pre-downloads the model), `status` (config + a healthz probe), `--version`.
//! The service-install lifecycle (`stop` / `uninstall` / `upgrade`, §10.3) rides
//! the shared service (M5) + self-update (M7) layers and is added there.
//!
//! The engine's inference backend is chosen at build time: the fail-loud
//! [`UnavailableBackend`] by default (a cmake-free build honestly can't infer),
//! and the native llama.cpp backend behind `--features llama`. When that backend
//! lands, `make_backend` becomes a `#[cfg(feature = "llama")]` branch.

mod server;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use modelstat_download::TtyProgress;
use modelstat_llm::{Backend, Engine, EngineConfig};
#[cfg(not(feature = "llama"))]
use modelstat_llm::UnavailableBackend;
use modelstat_sumclient::SummarizerClient;

/// The `summarizer-<semver>` banner string (§2).
const CLI_VERSION: &str = concat!("summarizer-", env!("CARGO_PKG_VERSION"));

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("modelstat-summarizer: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match args.first().map(String::as_str) {
        Some("serve") => rt.block_on(serve()),
        Some("setup") => rt.block_on(setup(&args[1..])),
        Some("status") => rt.block_on(status()),
        Some("stop") => cmd_stop(&args[1..]),
        Some("uninstall") => cmd_uninstall(&args[1..]),
        Some("upgrade") => rt.block_on(cmd_upgrade()),
        Some("--version") | Some("-v") | Some("version") => {
            println!("{CLI_VERSION}");
            ExitCode::SUCCESS
        }
        other => {
            if let Some(cmd) = other {
                eprintln!("modelstat-summarizer: unknown command `{cmd}`");
            }
            usage();
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!("{CLI_VERSION}");
    eprintln!("usage: modelstat-summarizer <serve|setup|status|stop|uninstall|upgrade|--version>");
    eprintln!("  serve      run the protocol-v1 inference server");
    eprintln!("  setup      configure summarizer.json + download the model + install the service");
    eprintln!("  status     show config + probe the running engine");
    eprintln!("  stop       stop the engine service (keeps it installed)");
    eprintln!("  uninstall  stop + remove the engine service (keeps model files)");
    eprintln!("  upgrade    self-update to the latest published release (§13)");
}

/// The install scope (`--system` = system-wide, else per-user).
fn arg_scope(args: &[String]) -> modelstat_service::Scope {
    if args.iter().any(|a| a == "--system") {
        modelstat_service::Scope::System
    } else {
        modelstat_service::Scope::User
    }
}

/// `modelstat-summarizer stop` — halt the engine service without removing it.
fn cmd_stop(args: &[String]) -> ExitCode {
    match modelstat_service::stop_service(modelstat_service::Component::Summarizer, arg_scope(args)) {
        Ok(()) => {
            println!("✓ summariser engine stopped");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("modelstat-summarizer: stop failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `modelstat-summarizer uninstall` — stop + remove the engine service. Model
/// files stay on disk (a re-`setup` reuses them).
fn cmd_uninstall(args: &[String]) -> ExitCode {
    match modelstat_service::uninstall_service(modelstat_service::Component::Summarizer, arg_scope(args)) {
        Ok(()) => {
            println!("✓ summariser engine service removed (model files kept)");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("modelstat-summarizer: uninstall failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `modelstat-summarizer upgrade` — the self-hosted-box self-update path (no
/// heartbeat verdict; resolves GitHub's latest directly, §13).
async fn cmd_upgrade() -> ExitCode {
    println!("upgrading the summariser engine to the latest published release…");
    match modelstat_update::upgrade_now().await {
        modelstat_update::UpgradeOutcome::Completed(note) => {
            println!("  ✓ {note}");
            ExitCode::SUCCESS
        }
        other => {
            if let Some(n) = other.note() {
                println!("  {n}");
            }
            ExitCode::SUCCESS
        }
    }
}

/// The engine's inference backend for this build. Default (cmake-free) = the
/// fail-loud [`UnavailableBackend`]; the native llama.cpp backend when built
/// `--features llama`. `models_dir` holds the GPU-abort guard the llama backend
/// consults.
#[cfg(feature = "llama")]
fn make_backend(models_dir: &std::path::Path) -> impl Backend {
    modelstat_llm::LlamaBackend::new(models_dir, env!("CARGO_PKG_VERSION"))
}

#[cfg(not(feature = "llama"))]
fn make_backend(_models_dir: &std::path::Path) -> impl Backend {
    UnavailableBackend
}

async fn serve() -> ExitCode {
    let home = home_dir();
    let models = models_dir(&home);
    let cfg = EngineConfig::load(&config_path(&home))
        .unwrap_or_else(|_| EngineConfig::defaults(&models));
    let engine = Arc::new(Engine::new(make_backend(&models), cfg.clone()));

    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("modelstat-summarizer: cannot bind {addr}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "modelstat-summarizer {CLI_VERSION} — protocol v1 on http://{addr} (backend: {})",
        engine.backend_name()
    );

    let app = server::router(engine.clone());
    let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    if let Err(e) = serve.await {
        eprintln!("modelstat-summarizer: server error: {e}");
        engine.shutdown().await;
        return ExitCode::FAILURE;
    }
    // Refuse new work, drain in-flight ≤8s, free the backend (§6.8/§10.2).
    engine.shutdown().await;
    ExitCode::SUCCESS
}

/// `setup` (feature §10.3). Two shapes:
///   - `--loopback` (driven by `connect`/`mode local`): write `summarizer.json`
///     defaults + download the model. No prompts, no service (the collector arms
///     the loopback service itself).
///   - standalone (the installer's `summarizer` component, an org engine box):
///     prompt bind/port (`0.0.0.0` needs an explicit confirm), download, install
///     the service, ask about daily auto-update, print the collector command.
async fn setup(args: &[String]) -> ExitCode {
    let loopback = args.iter().any(|a| a == "--loopback");
    let interactive =
        !loopback && !args.iter().any(|a| a == "--yes") && std::io::stdin().is_terminal();
    let home = home_dir();
    let models = models_dir(&home);
    if let Err(e) = std::fs::create_dir_all(&models) {
        eprintln!("modelstat-summarizer: cannot create {}: {e}", models.display());
        return ExitCode::FAILURE;
    }
    let cfg_path = config_path(&home);
    let mut cfg = EngineConfig::load(&cfg_path).unwrap_or_else(|_| EngineConfig::defaults(&models));

    // Resolve bind + port: flags always; else prompt (standalone interactive);
    // else keep the loaded/default values.
    if let Some(b) = flag_value(args, "--bind") {
        cfg.bind = b;
    } else if interactive {
        cfg.bind = prompt(&format!("Bind address [{}]: ", cfg.bind), &cfg.bind);
    }
    if cfg.bind == "0.0.0.0" && interactive {
        eprintln!("⚠ 0.0.0.0 exposes the engine on the network — it has NO authentication.");
        eprintln!("  Only do this behind a reverse proxy / firewall you trust.");
        if prompt("  Type 'expose' to confirm: ", "") != "expose" {
            eprintln!("  aborting — re-run and choose 127.0.0.1 to bind to loopback only.");
            return ExitCode::FAILURE;
        }
    }
    if let Some(p) = flag_value(args, "--port").and_then(|v| v.parse::<u16>().ok()) {
        cfg.port = p;
    } else if interactive {
        let p = prompt(&format!("Port [{}]: ", cfg.port), &cfg.port.to_string());
        if let Ok(p) = p.parse::<u16>() {
            cfg.port = p;
        }
    }

    if let Err(e) = cfg.save(&cfg_path) {
        eprintln!("modelstat-summarizer: cannot write {}: {e}", cfg_path.display());
        return ExitCode::FAILURE;
    }
    eprintln!("▸ wrote {}", cfg_path.display());

    // Pre-download the model with progress. A failure NEVER fails setup — the
    // engine lazy-downloads on first use and self-heals (§11).
    let client = reqwest::Client::new();
    let spec = cfg.download_spec();
    eprintln!(
        "▸ model: {} ({})",
        spec.url,
        spec.size_label.as_deref().unwrap_or("size unknown")
    );
    match modelstat_download::download(&client, &spec, &TtyProgress::new("Qwen3.5-4B")).await {
        Ok(p) => eprintln!("✓ model ready: {}", p.display()),
        Err(e) => eprintln!("⚠ model download deferred (lazy-downloads on first use): {e}"),
    }

    // Loopback (collector-driven): the collector installs + bounces the service.
    if loopback {
        return ExitCode::SUCCESS;
    }

    // Standalone engine box: install the service + configure daily auto-update.
    let scope = arg_scope(args);
    match modelstat_service::install_service(modelstat_service::Component::Summarizer, scope) {
        Ok(svc) => eprintln!("✓ engine service installed: {}", svc.path.display()),
        Err(e) => eprintln!("⚠ couldn't install the engine service ({e}) — run `serve` manually"),
    }
    if interactive {
        // Default OFF — a shared box should change predictably (§10.3/§13).
        let daily = prompt("Enable daily auto-update? [y/N]: ", "n");
        let on = matches!(daily.trim().to_lowercase().as_str(), "y" | "yes");
        let _ = modelstat_update::set_stored_auto_update(on);
    }

    println!();
    println!("Collectors point at this engine with:");
    println!("  modelstat mode self-hosted --url http://{}:{}", cfg.bind, cfg.port);
    ExitCode::SUCCESS
}

/// `--flag v` / `--flag=v` reader.
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

/// Read a line from stdin; empty input (or a closed stdin) yields `default`.
fn prompt(msg: &str, default: &str) -> String {
    use std::io::Write;
    print!("{msg}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => default.to_string(),
        Ok(_) => {
            let v = line.trim();
            if v.is_empty() {
                default.to_string()
            } else {
                v.to_string()
            }
        }
    }
}

async fn status() -> ExitCode {
    let home = home_dir();
    let cfg = match EngineConfig::load(&config_path(&home)) {
        Ok(cfg) => cfg,
        Err(_) => {
            println!("not configured — run `modelstat-summarizer setup`");
            return ExitCode::SUCCESS;
        }
    };
    println!("config: {}", config_path(&home).display());
    println!("  bind:  {}:{}", cfg.bind, cfg.port);
    println!("  model: {}", cfg.model_path.display());
    println!(
        "  model present: {}",
        if cfg.model_path.exists() { "yes" } else { "no (lazy)" }
    );

    let client = SummarizerClient::new(format!("http://{}:{}", cfg.bind, cfg.port));
    match client.healthz().await {
        Ok(h) => println!(
            "engine: UP — backend {}, model_loaded {}, protocol {}",
            h.backend, h.model_loaded, h.protocol
        ),
        Err(e) => println!("engine: not reachable ({e})"),
    }
    ExitCode::SUCCESS
}

/// Resolve until SIGTERM (service stop) or Ctrl-C (SIGINT).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    eprintln!("modelstat-summarizer: shutting down (draining ≤8s)…");
}

fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("MODELSTAT_HOME") {
        return PathBuf::from(h);
    }
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".modelstat")
}

fn models_dir(home: &std::path::Path) -> PathBuf {
    std::env::var_os("MODELSTAT_MODELS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("models"))
}

fn config_path(home: &std::path::Path) -> PathBuf {
    home.join("summarizer.json")
}
