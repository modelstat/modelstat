//! Run-verify the real llama.cpp engine end to end: download the pinned Qwen
//! GGUF, load it (Metal on macOS / CPU elsewhere), and generate a real summary
//! from a sample prompt — proving the engine actually produces text.
//!
//!   cargo run -p modelstat-summarizer --example llama_verify --features llama
//!
//! Downloads ~2.7 GB into a temp dir on first run (cached after). This is the
//! "does the real engine WORK" check the compile can't give you.

#[cfg(not(feature = "llama"))]
fn main() {
    eprintln!("re-run with `--features llama` (needs cmake) to verify the llama engine.");
    std::process::exit(2);
}

#[cfg(feature = "llama")]
#[tokio::main]
async fn main() {
    use modelstat_download::{download, TtyProgress};
    use modelstat_llm::{strip_think, Backend, EngineConfig, GenParams, LlamaBackend};

    let models_dir = std::env::temp_dir().join("modelstat-llama-verify");
    let cfg = EngineConfig::defaults(&models_dir);
    let spec = cfg.download_spec();

    println!(
        "▸ downloading the Qwen model ({}) into {}",
        spec.size_label.as_deref().unwrap_or("~2.7 GB"),
        models_dir.display()
    );
    if let Err(e) = download(
        &reqwest::Client::new(),
        &spec,
        &TtyProgress::new("Qwen3.5-4B"),
    )
    .await
    {
        eprintln!("✗ download failed: {e}");
        std::process::exit(1);
    }

    let mut backend = LlamaBackend::new(&models_dir, env!("CARGO_PKG_VERSION"));
    println!("\n▸ loading model (backend: {})…", backend.backend_name());
    if let Err(e) = backend.load(&cfg.model_path, cfg.context) {
        eprintln!("✗ load failed: {e}");
        std::process::exit(1);
    }

    // A generic summarize-shaped prompt (the collector owns the real §18 prompts;
    // the engine is prompt-agnostic — this just proves it generates text).
    let params = GenParams {
        system: "You summarise an AI coding session in 1-2 sentences, \u{2264} 400 characters. \
                 Lead with an outcome verb; name the concrete work. Reply with only the summary."
            .to_string(),
        user: "Facts: repo acme/web, branch main, 4 turns.\nExcerpts:\n\
               - Added a retry loop with exponential backoff to the ingest uploader.\n\
               - Fixed a null dereference in the auth middleware of api-gateway."
            .to_string(),
        temperature: 0.2,
        max_tokens: 512,
        top_k: Some(3),
    };

    println!("\n▸ generating a summary…");
    match backend.generate(&params) {
        Ok(raw) => {
            let clean = strip_think(&raw);
            println!(
                "\n=== raw output (first 300 chars) ===\n{}",
                raw.chars().take(300).collect::<String>()
            );
            println!("\n=== summary (<think> stripped) ===\n{clean}");
            if clean.trim().is_empty() {
                eprintln!("\n✗ engine produced no answer after stripping <think>");
                std::process::exit(1);
            }
            println!("\n✓ llama engine run-verified");
        }
        Err(e) => {
            eprintln!("✗ generate failed: {e}");
            std::process::exit(1);
        }
    }
}
