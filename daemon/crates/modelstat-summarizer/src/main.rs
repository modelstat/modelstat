//! `modelstat-summarizer` — the inference-engine binary entry point.
//!
//! The protocol-v1 axum server, llama.cpp integration, lazy load / idle unload,
//! GPU guard, and the setup/serve/status/stop/uninstall/upgrade commands land in
//! M3/M6 (plan §5). M0 provides a compiling entry point and, crucially,
//! establishes this as the sole binary in the workspace that links modelstat-llm.

const VERSION: &str = concat!("summarizer-", env!("CARGO_PKG_VERSION"));

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") | Some("-v") | Some("version") => println!("{VERSION}"),
        _ => {
            println!("{VERSION}");
            println!("the summarizer engine is implemented in milestones M3/M6");
        }
    }
}
