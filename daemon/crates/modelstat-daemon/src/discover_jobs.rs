//! Scan-job discovery — a port of `apps/daemon/src/scan.ts`'s `discoverJobs` +
//! `orderJobsNewestFirst`. Walks the known transcript roots and yields one
//! [`ScanJob`] per `.jsonl`, newest-first so a session you just finished uploads
//! within seconds instead of behind a backlog.
//!
//! Deliberately a direct filesystem walk (not the richer `discover()` installer
//! probe): the scan + the self-healing reconcile must reason over the exact same
//! file set, and the per-tool directory shapes are fixed. Cursor is dormant (§7.1)
//! and not walked here.

use std::path::{Path, PathBuf};

use modelstat_parsers::{
    parse_claude_code_jsonl, parse_claude_code_jsonl_streaming, parse_codex_rollout,
    parse_codex_rollout_streaming, parse_pi_session, parse_pi_session_streaming, ParseResult,
    ParserContext,
};
use modelstat_wire::RawEvent;

/// Which parser a discovered transcript needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserKind {
    ClaudeCode,
    Codex,
    Pi,
}

/// One transcript to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanJob {
    pub path: String,
    pub kind: ParserKind,
}

/// `$HOME` (or `%USERPROFILE%`), matching the parsers' own home probe.
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|s| !s.is_empty()))
        .map(PathBuf::from)
}

fn child_paths(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

fn is_jsonl(p: &Path) -> bool {
    p.extension().map(|e| e == "jsonl").unwrap_or(false)
}

fn is_rollout_jsonl(p: &Path) -> bool {
    is_jsonl(p)
        && p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("rollout-"))
            .unwrap_or(false)
}

/// Discover every scan job under `home` (the 3 active parsers). Test-injectable
/// root; [`discover_jobs`] passes the real home.
pub fn discover_jobs_in(home: &Path) -> Vec<ScanJob> {
    let mut jobs = Vec::new();

    // Claude Code — ~/.claude/projects/<p>/*.jsonl
    for proj in child_paths(&home.join(".claude/projects")) {
        if proj.is_dir() {
            for f in child_paths(&proj) {
                if is_jsonl(&f) {
                    jobs.push(ScanJob {
                        path: f.to_string_lossy().into_owned(),
                        kind: ParserKind::ClaudeCode,
                    });
                }
            }
        }
    }

    // Codex — ~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl
    for y in child_paths(&home.join(".codex/sessions")) {
        for m in child_paths(&y) {
            for d in child_paths(&m) {
                for f in child_paths(&d) {
                    if is_rollout_jsonl(&f) {
                        jobs.push(ScanJob {
                            path: f.to_string_lossy().into_owned(),
                            kind: ParserKind::Codex,
                        });
                    }
                }
            }
        }
    }

    // pi — ~/.pi/agent/sessions/<p>/*.jsonl
    for proj in child_paths(&home.join(".pi/agent/sessions")) {
        if proj.is_dir() {
            for f in child_paths(&proj) {
                if is_jsonl(&f) {
                    jobs.push(ScanJob {
                        path: f.to_string_lossy().into_owned(),
                        kind: ParserKind::Pi,
                    });
                }
            }
        }
    }

    jobs
}

/// Discover every scan job under the real `$HOME`.
pub fn discover_jobs() -> Vec<ScanJob> {
    home_dir().map(|h| discover_jobs_in(&h)).unwrap_or_default()
}

/// Parse one job (collect mode) with the right parser.
pub fn parse_job(device_id: &str, job: &ScanJob) -> std::io::Result<ParseResult> {
    let ctx = ParserContext::new(device_id, job.path.clone());
    match job.kind {
        ParserKind::ClaudeCode => parse_claude_code_jsonl(&ctx),
        ParserKind::Codex => parse_codex_rollout(&ctx),
        ParserKind::Pi => parse_pi_session(&ctx),
    }
}

/// Parse one job in STREAMING mode: events flow to `emit` in bounded ≤256-event
/// chunks (never fully materialised), and the returned [`ParseResult`] carries the
/// collected tool-call drafts + script contexts + stats with `events` empty. This
/// is what the scan loop drives (via a spawn-blocking + bounded-channel bridge) so
/// a multi-hundred-MB transcript stays within a fixed memory ceiling. Mirrors
/// [`parse_job`]'s parser dispatch.
pub fn parse_job_streaming(
    device_id: &str,
    job: &ScanJob,
    emit: &mut dyn FnMut(Vec<RawEvent>),
) -> std::io::Result<ParseResult> {
    let ctx = ParserContext::new(device_id, job.path.clone());
    match job.kind {
        ParserKind::ClaudeCode => parse_claude_code_jsonl_streaming(&ctx, emit),
        ParserKind::Codex => parse_codex_rollout_streaming(&ctx, emit),
        ParserKind::Pi => parse_pi_session_streaming(&ctx, emit),
    }
}

/// Sort newest-first by file mtime so a just-finished session uploads first.
pub fn order_jobs_newest_first(mut jobs: Vec<ScanJob>) -> Vec<ScanJob> {
    jobs.sort_by(|a, b| mtime_ms(&b.path).cmp(&mtime_ms(&a.path)));
    jobs
}

fn mtime_ms(path: &str) -> u128 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_the_three_roots_and_tags_the_parser() {
        let home = std::env::temp_dir().join(format!("modelstat-jobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mk = |rel: &str| {
            let p = home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"{}").unwrap();
        };
        mk(".claude/projects/proj1/a.jsonl");
        mk(".claude/projects/proj1/notes.txt"); // ignored (not .jsonl)
        mk(".codex/sessions/2026/07/16/rollout-abc.jsonl");
        mk(".codex/sessions/2026/07/16/other.jsonl"); // ignored (not rollout-)
        mk(".pi/agent/sessions/p/b.jsonl");

        let jobs = discover_jobs_in(&home);
        assert_eq!(jobs.len(), 3);
        assert!(jobs
            .iter()
            .any(|j| j.kind == ParserKind::ClaudeCode && j.path.ends_with("a.jsonl")));
        assert!(jobs
            .iter()
            .any(|j| j.kind == ParserKind::Codex && j.path.ends_with("rollout-abc.jsonl")));
        assert!(jobs
            .iter()
            .any(|j| j.kind == ParserKind::Pi && j.path.ends_with("b.jsonl")));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_roots_yield_no_jobs() {
        let home = std::env::temp_dir().join(format!("modelstat-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert!(discover_jobs_in(&home).is_empty());
    }
}
