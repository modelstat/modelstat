//! Scan-job discovery — a port of `apps/daemon/src/scan.ts`'s `discoverJobs` +
//! `orderJobsNewestFirst`. Walks the known transcript roots and yields one
//! [`ScanJob`] per `.jsonl`, newest-first so a session you just finished uploads
//! within seconds instead of behind a backlog.
//!
//! Deliberately a direct filesystem walk (not the richer `discover()` installer
//! probe): the scan + the self-healing reconcile must reason over the exact same
//! file set, and the per-tool directory shapes are fixed.
//!
//! Four agents are walked: Claude Code, Codex, pi/omp, and Cursor. Cursor is the
//! odd one — its conversations live in ONE global key/value DB rather than
//! per-session transcript files, so its floor is a timestamp watermark rather
//! than a byte offset (see `ScanJob::since_ms`).

use std::path::{Path, PathBuf};

use modelstat_parsers::{
    auth_mode, parse_claude_code_jsonl, parse_claude_code_jsonl_streaming, parse_codex_rollout,
    parse_codex_rollout_streaming, parse_cursor_tracking_db, parse_pi_session,
    parse_pi_session_streaming, ParseResult, ParserContext,
};
use modelstat_wire::RawEvent;

/// Which parser a discovered transcript needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserKind {
    ClaudeCode,
    Codex,
    Pi,
    Cursor,
}

/// One transcript to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanJob {
    pub path: String,
    pub kind: ParserKind,
    /// Only for non-positional sources ([`ParserKind::Cursor`]): skip records
    /// already shipped through this instant. The scan fills it from the file's
    /// cursor; discovery always yields `None`.
    pub since_ms: Option<i64>,
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
                        since_ms: None,
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
                            since_ms: None,
                        });
                    }
                }
            }
        }
    }

    // pi / omp (Oh My Pi) — <home>/.{pi,omp}/agent/sessions/<p>/*.jsonl. OMP is Pi
    // with its home relocated to ~/.omp; identical JSONL format + parser. One level
    // deep by design: top-level <TS>_<uuid>.jsonl are the session transcripts,
    // while nested <TS>_<uuid>/ dirs (subagent + tool logs) are skipped so a
    // subagent's tokens aren't double-counted against its parent.
    for root in [".pi/agent/sessions", ".omp/agent/sessions"] {
        for proj in child_paths(&home.join(root)) {
            if proj.is_dir() {
                for f in child_paths(&proj) {
                    if is_jsonl(&f) {
                        jobs.push(ScanJob {
                            since_ms: None,
                            path: f.to_string_lossy().into_owned(),
                            kind: ParserKind::Pi,
                        });
                    }
                }
            }
        }
    }

    // Cursor — the chat store is ONE global key/value DB, not a directory of
    // per-session transcripts: `<user-data>/User/globalStorage/state.vscdb`,
    // whose location is per-OS. Workspace DBs hold no conversations.
    for rel in CURSOR_DB_RELATIVE_PATHS {
        let db = home.join(rel);
        if db.is_file() {
            jobs.push(ScanJob {
                path: db.to_string_lossy().into_owned(),
                kind: ParserKind::Cursor,
                // Filled by the scan from the file's cursor.
                since_ms: None,
            });
        }
    }

    jobs
}

/// Where Cursor keeps its global storage, per platform. All three are probed —
/// a wrong-platform path simply does not exist, and probing beats a `cfg!` when
/// a user runs Cursor under a translation layer.
const CURSOR_DB_RELATIVE_PATHS: &[&str] = &[
    // macOS
    "Library/Application Support/Cursor/User/globalStorage/state.vscdb",
    // Linux
    ".config/Cursor/User/globalStorage/state.vscdb",
    // Windows (%APPDATA% sits under the user profile)
    "AppData/Roaming/Cursor/User/globalStorage/state.vscdb",
];

/// Discover every scan job under the real `$HOME`.
pub fn discover_jobs() -> Vec<ScanJob> {
    home_dir().map(|h| discover_jobs_in(&h)).unwrap_or_default()
}

/// How the agent behind `kind` authenticates on this machine — the value every
/// event from this job carries, and the only thing that decides whether its
/// tokens are billable.
///
/// Read here, once per job, rather than inside the parse loop: it is a property
/// of the machine's login, so re-reading it per transcript line would be pure
/// syscall waste. Resolves to `unknown` when `$HOME` itself is unreadable —
/// there is nowhere left to look, and inventing a mode from no evidence is the
/// exact failure this replaced.
fn pricing_mode_for(kind: ParserKind) -> String {
    let home = home_dir()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    match kind {
        ParserKind::ClaudeCode => auth_mode::claude_code_pricing_mode(&home),
        ParserKind::Codex => auth_mode::codex_pricing_mode(&home),
        ParserKind::Pi => auth_mode::pi_pricing_mode(),
        // Cursor bills its own flat plan; its rows carry no tokens either way.
        ParserKind::Cursor => auth_mode::PRICING_MODE_SUBSCRIPTION,
    }
    .to_string()
}

/// Parse one job (collect mode) with the right parser.
pub fn parse_job(device_id: &str, job: &ScanJob) -> std::io::Result<ParseResult> {
    let ctx = ParserContext::new(device_id, job.path.clone())
        .with_pricing_mode(pricing_mode_for(job.kind))
        .with_since_ms(job.since_ms);
    match job.kind {
        ParserKind::ClaudeCode => parse_claude_code_jsonl(&ctx),
        ParserKind::Codex => parse_codex_rollout(&ctx),
        ParserKind::Pi => parse_pi_session(&ctx),
        ParserKind::Cursor => parse_cursor_tracking_db(&ctx),
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
    let ctx = ParserContext::new(device_id, job.path.clone())
        .with_pricing_mode(pricing_mode_for(job.kind))
        .with_since_ms(job.since_ms);
    match job.kind {
        ParserKind::ClaudeCode => parse_claude_code_jsonl_streaming(&ctx, emit),
        ParserKind::Codex => parse_codex_rollout_streaming(&ctx, emit),
        ParserKind::Pi => parse_pi_session_streaming(&ctx, emit),
        // A key/value store has no streaming shape: it is read whole (bounded by
        // the since-floor) and handed over as one chunk.
        ParserKind::Cursor => {
            let mut r = parse_cursor_tracking_db(&ctx)?;
            let events = std::mem::take(&mut r.events);
            if !events.is_empty() {
                emit(events);
            }
            Ok(r)
        }
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
    fn walks_the_transcript_roots_and_tags_the_parser() {
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
        mk(".omp/agent/sessions/p/c.jsonl");

        let jobs = discover_jobs_in(&home);
        assert_eq!(jobs.len(), 4);
        assert!(jobs
            .iter()
            .any(|j| j.kind == ParserKind::ClaudeCode && j.path.ends_with("a.jsonl")));
        assert!(jobs
            .iter()
            .any(|j| j.kind == ParserKind::Codex && j.path.ends_with("rollout-abc.jsonl")));
        assert!(jobs
            .iter()
            .any(|j| j.kind == ParserKind::Pi && j.path.ends_with("b.jsonl")));
        assert!(jobs
            .iter()
            .any(|j| j.kind == ParserKind::Pi && j.path.ends_with("c.jsonl")));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_roots_yield_no_jobs() {
        let home = std::env::temp_dir().join(format!("modelstat-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        assert!(discover_jobs_in(&home).is_empty());
    }
}

#[cfg(test)]
mod cursor_discovery_tests {
    use super::*;

    /// Cursor's chat store is discovered where it actually lives, on every
    /// platform's user-data path. It went unwalked for the parser's whole
    /// life — the docs claimed an env flag gated it, and no such flag existed.
    #[test]
    fn cursor_global_storage_is_discovered_on_each_platform_layout() {
        for rel in CURSOR_DB_RELATIVE_PATHS {
            let home = std::env::temp_dir().join(format!(
                "modelstat-disco-{}-{}",
                std::process::id(),
                rel.replace(['/', ' '], "_")
            ));
            let db = home.join(rel);
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            std::fs::write(&db, b"not-a-real-db").unwrap();

            let jobs = discover_jobs_in(&home);
            let cursor: Vec<_> = jobs
                .iter()
                .filter(|j| j.kind == ParserKind::Cursor)
                .collect();
            assert_eq!(cursor.len(), 1, "exactly one cursor job for {rel}");
            assert_eq!(cursor[0].path, db.to_string_lossy());
            assert_eq!(cursor[0].since_ms, None, "discovery never sets the floor");
            std::fs::remove_dir_all(&home).ok();
        }
    }

    #[test]
    fn a_home_without_cursor_yields_no_cursor_job() {
        let home =
            std::env::temp_dir().join(format!("modelstat-disco-empty-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        assert!(!discover_jobs_in(&home)
            .iter()
            .any(|j| j.kind == ParserKind::Cursor));
        std::fs::remove_dir_all(&home).ok();
    }
}
