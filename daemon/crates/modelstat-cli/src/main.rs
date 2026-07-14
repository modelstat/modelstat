//! `modelstat` — the collector binary entry point.
//!
//! The full CLI surface (connect, start, mcp, statusline, status/jobs/paths/…)
//! lands across M1/M4/M5/M6 (plan §5). M0 provides a compiling entry point so
//! the six-target build matrix is green and the crate graph is real.

/// Compile-time version string, `daemon-<semver>` (feature §5).
const VERSION: &str = concat!("daemon-", env!("CARGO_PKG_VERSION"));

fn main() {
    // Minimal real behavior: report version. Subcommand dispatch arrives in M1.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") | Some("-v") | Some("version") => println!("{VERSION}"),
        _ => {
            println!("{VERSION}");
            println!("the collector CLI is implemented across milestones M1–M6");
        }
    }
}
