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

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use modelstat_download::TtyProgress;
use modelstat_llm::{Backend, Engine, EngineConfig, UnavailableBackend};
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
        Some("setup") => rt.block_on(setup()),
        Some("status") => rt.block_on(status()),
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
    eprintln!("usage: modelstat-summarizer <serve|setup|status|--version>");
    eprintln!("  serve    run the protocol-v1 inference server");
    eprintln!("  setup    write summarizer.json + pre-download the model");
    eprintln!("  status   show config + probe the running engine");
}

/// The engine's inference backend for this build. Default (cmake-free) = the
/// fail-loud [`UnavailableBackend`]; the native llama.cpp backend slots in here
/// behind the `llama` feature.
fn make_backend() -> impl Backend {
    UnavailableBackend
}

async fn serve() -> ExitCode {
    let home = home_dir();
    let cfg = EngineConfig::load(&config_path(&home))
        .unwrap_or_else(|_| EngineConfig::defaults(&models_dir(&home)));
    let engine = Arc::new(Engine::new(make_backend(), cfg.clone()));

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

async fn setup() -> ExitCode {
    let home = home_dir();
    let models = models_dir(&home);
    if let Err(e) = std::fs::create_dir_all(&models) {
        eprintln!("modelstat-summarizer: cannot create {}: {e}", models.display());
        return ExitCode::FAILURE;
    }
    let cfg_path = config_path(&home);
    let cfg = EngineConfig::load(&cfg_path).unwrap_or_else(|_| EngineConfig::defaults(&models));
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

    println!();
    println!("Collectors point at this engine with:");
    println!("  modelstat mode self-hosted --url http://{}:{}", cfg.bind, cfg.port);
    ExitCode::SUCCESS
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
