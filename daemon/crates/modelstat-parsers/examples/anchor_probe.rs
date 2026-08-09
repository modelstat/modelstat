//! Run the anchor miner against real repositories and print what it found.
//!
//! This exists because the miner's first cut passed every unit test and still
//! returned ZERO anchors on every real repo — its "pre-AI" date cutoff predated
//! the repos themselves. A synthetic fixture cannot catch that; a real one can.
//!
//!   cargo run -p modelstat-parsers --example anchor_probe -- ~/src/some-repo …
//!
//! Prints only what the wire would carry (slug, counts, PR numbers, shas,
//! timestamps) plus the argument path, which never leaves this terminal.
//! `MODELSTAT_ANCHOR_CUTOFF` is honored so the demoted date filter can be
//! exercised too.

use modelstat_parsers::{mine_repo_anchors, AnchorConfig};

fn main() {
    let cutoff = std::env::var("MODELSTAT_ANCHOR_CUTOFF")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let cfg = AnchorConfig {
        cutoff,
        ..Default::default()
    };
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: anchor_probe <repo-path> [<repo-path>…]");
        std::process::exit(2);
    }
    for path in paths {
        let started = std::time::Instant::now();
        let Some(repo) = mine_repo_anchors(&path, &cfg) else {
            println!("{path}: no mine (not a git repo, no remote slug, or git failed)");
            continue;
        };
        println!(
            "{} head={} cutoff={} human_anchor_count={} ai_pr_count={} in {:?}",
            repo.slug,
            short(&repo.head_sha),
            repo.cutoff.as_deref().unwrap_or("none"),
            repo.human_anchor_count,
            repo.ai_pr_count,
            started.elapsed(),
        );
        for a in repo.anchors.iter().take(3) {
            println!(
                "  #{:<6} {}  files={:<4} +{:<6} -{:<6} span_ms={:<12} active_minutes={:<8} ai_assisted={}",
                a.pr_number,
                a.merged_at,
                a.files_changed,
                a.lines_added,
                a.lines_deleted,
                opt(a.span_ms),
                opt(a.active_minutes),
                a.ai_assisted,
            );
        }
        if repo.anchors.is_empty() {
            println!("  (no human-authored anchors)");
        }
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_string(), |v| v.to_string())
}
