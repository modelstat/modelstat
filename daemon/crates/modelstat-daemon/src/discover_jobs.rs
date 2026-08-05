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
    /// Overrides the agent the parser stamps on every event. Set when a HOST
    /// runs another agent's binary — Claude Desktop's local agent mode is
    /// Claude Code by format, but the human used Claude Desktop, and `agent`
    /// names the tool the human used. `None` keeps the parser's own name.
    pub agent_label: Option<String>,
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

/// How deep to hunt for a `.claude/projects` tree under a search root. Claude
/// Desktop nests it four levels down (`local-agent-mode-sessions/<a>/<b>/
/// local_<c>/.claude`); the cap keeps an unlucky root — a multi-GB app-data
/// directory full of caches and VM images — from turning a scan into a full
/// disk walk.
const CLAUDE_SEARCH_MAX_DEPTH: usize = 5;

/// Directory names never worth descending: vendor caches, blob stores and VM
/// images that hold no transcripts and plenty of gigabytes.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "blob_storage",
    "Cache",
    "Code Cache",
    "GPUCache",
    "CachedData",
    "Crashpad",
    "logs",
    "claude-code-vm",
    "Partitions",
    "Service Worker",
    "IndexedDB",
    "Local Storage",
];

/// Where to hunt for Claude Code transcript trees, and the agent label sessions
/// found under each should carry.
///
/// The label matters: a Desktop-hosted session IS Claude Code by format, but
/// the human used **Claude Desktop**, and `agent` names the tool the human
/// used. Merging them would destroy a distinction nothing can recover later;
/// keeping them apart can always be summed at read time.
fn claude_search_roots(home: &Path) -> Vec<(PathBuf, Option<&'static str>, usize)> {
    // The CLI's own home.
    // Depth 0 for the homes: `<home>/.claude/projects` is an exact location, and
    // descending from a home that happens to lack `.claude` would walk the
    // user's entire disk — and rediscover, unlabelled, every hosted tree the
    // app-data roots below already own.
    let mut roots: Vec<(PathBuf, Option<&'static str>, usize)> =
        vec![(home.to_path_buf(), None, 0)];
    // `CLAUDE_CONFIG_DIR` relocates that home; a user who set it would
    // otherwise report nothing at all. It points AT the `.claude` dir, so the
    // shape search starts at its parent.
    if let Some(parent) = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .and_then(|c| Path::new(&c).parent().map(Path::to_path_buf))
    {
        roots.push((parent, None, 0));
    }
    // Every application's data directory, per platform — NOT a list of app
    // names. An app's data dir is named after the app, and there is no way to
    // know that name in advance: a second install is "Claude Second", a beta is
    // "Claude Dev", a fork is anything at all. So each one is probed for the
    // transcript shape, and whether it counts as a host is decided by what is
    // INSIDE it, never by what it is called.
    for app_data in [
        home.join("Library/Application Support"),
        home.join(".config"),
        home.join("AppData/Roaming"),
    ] {
        for app in child_paths(&app_data) {
            let name = app.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || SKIP_DIRS.contains(&name) || !app.is_dir() {
                continue;
            }
            let label = desktop_host_label(&app);
            roots.push((app, label, CLAUDE_SEARCH_MAX_DEPTH));
        }
    }
    roots.retain(|(p, _, _)| p.is_dir());
    roots
}

/// Does this application data directory belong to a Claude Desktop-style host?
///
/// Decided by ARTEFACTS, not by the directory's name: Desktop keeps its
/// per-session sandboxes in `local-agent-mode-sessions` and its session index
/// in `claude-code-sessions`. A second install under any name carries the same
/// markers; something else that merely happens to host a Claude Code tree does
/// not, and is left unlabelled — honestly reported as plain Claude Code rather
/// than as a desktop app we only guessed at.
fn desktop_host_label(app_data: &Path) -> Option<&'static str> {
    ["local-agent-mode-sessions", "claude-code-sessions"]
        .iter()
        .any(|marker| app_data.join(marker).is_dir())
        .then_some("claude_desktop")
}

/// Walk `root` (bounded) for `.claude/projects/<dir>/*.jsonl` transcripts.
///
/// Only the `projects` tree is taken. A Desktop session directory also holds an
/// `audit.jsonl` beside it, and that file is the SAME conversation in the
/// Agent-SDK's shape — verified on a real install, where every message text and
/// 21 of 25 line uuids appear in both. Walking both would double every message
/// and every token the session cost.
fn collect_claude_projects(
    root: &Path,
    depth_left: usize,
    agent: Option<&'static str>,
    jobs: &mut Vec<ScanJob>,
) {
    let projects = root.join(".claude/projects");
    if projects.is_dir() {
        for proj in child_paths(&projects) {
            if proj.is_dir() {
                for f in child_paths(&proj) {
                    if is_jsonl(&f) {
                        jobs.push(ScanJob {
                            path: f.to_string_lossy().into_owned(),
                            kind: ParserKind::ClaudeCode,
                            since_ms: None,
                            agent_label: agent.map(str::to_string),
                        });
                    }
                }
            }
        }
        // A transcript tree never nests another; stop descending this branch.
        return;
    }
    if depth_left == 0 {
        return;
    }
    for child in child_paths(root) {
        let name = child.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // `.claude` is handled above; skip other dotdirs and the heavyweights.
        if name.starts_with('.') || SKIP_DIRS.contains(&name) || !child.is_dir() {
            continue;
        }
        collect_claude_projects(&child, depth_left - 1, agent, jobs);
    }
}

/// Discover every scan job under `home` (the 3 active parsers). Test-injectable
/// root; [`discover_jobs`] passes the real home.
pub fn discover_jobs_in(home: &Path) -> Vec<ScanJob> {
    let mut jobs = Vec::new();

    // Claude Code — ~/.claude/projects/<encoded-cwd>/<session>.jsonl, plus the
    // same tree wherever an EMBEDDER puts it. Searched by SHAPE rather than by
    // one hard-coded path: Claude Desktop's local agent mode runs Claude Code
    // with a relocated home, so its transcripts sit at
    // `<app-data>/local-agent-mode-sessions/<a>/<b>/local_<c>/.claude/projects/...`
    // and were invisible for as long as only `$HOME` was walked. Anything else
    // hosting Claude Code the same way is picked up for free.
    for (root, agent, depth) in claude_search_roots(home) {
        collect_claude_projects(&root, depth, agent, &mut jobs);
    }
    // Belt and braces: one transcript, one job. Roots can overlap (a relocated
    // CLAUDE_CONFIG_DIR living inside an app's data dir, say), and scanning a
    // file twice would parse and upload it twice every cycle. A labelled job
    // wins, being the more specific claim about who ran the session.
    jobs.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| b.agent_label.is_some().cmp(&a.agent_label.is_some()))
    });
    jobs.dedup_by(|a, b| a.path == b.path);

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
                            agent_label: None,
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
                            agent_label: None,
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
                agent_label: None,
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
        .with_since_ms(job.since_ms)
        .with_agent_label(job.agent_label.clone());
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
        .with_since_ms(job.since_ms)
        .with_agent_label(job.agent_label.clone());
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

    /// The app's data directory is named after the app, and there is no way to
    /// know that name: a second install is "Claude Second", a beta is "Claude
    /// Dev", a fork is anything at all. Discovery must key on ARTEFACTS.
    #[test]
    fn an_arbitrarily_named_app_hosting_claude_code_is_found_and_labelled() {
        let home = std::env::temp_dir().join(format!("modelstat-host-{}", std::process::id()));
        let app = home.join("Library/Application Support/Totally Unknown Name");
        // The Desktop marker, plus a transcript nested exactly as Desktop nests it.
        std::fs::create_dir_all(app.join("local-agent-mode-sessions")).unwrap();
        let proj = app.join("local-agent-mode-sessions/a/b/local_c/.claude/projects/-enc");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("s1.jsonl"), b"{}").unwrap();
        // The sibling audit log the Agent SDK writes — the same conversation,
        // and taking it too would double every message.
        std::fs::write(
            app.join("local-agent-mode-sessions/a/b/local_c/audit.jsonl"),
            b"{}",
        )
        .unwrap();

        let jobs = discover_jobs_in(&home);
        let hosted: Vec<_> = jobs.iter().filter(|j| j.agent_label.is_some()).collect();
        assert_eq!(hosted.len(), 1, "found regardless of the app's name");
        assert_eq!(hosted[0].agent_label.as_deref(), Some("claude_desktop"));
        assert!(hosted[0].path.ends_with("s1.jsonl"));
        assert!(
            !jobs.iter().any(|j| j.path.ends_with("audit.jsonl")),
            "the duplicate audit log is never walked"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// No Desktop artefacts → still discovered (it IS Claude Code), but not
    /// labelled as a desktop app we only guessed at.
    #[test]
    fn a_hosted_tree_without_desktop_markers_stays_unlabelled() {
        let home = std::env::temp_dir().join(format!("modelstat-host2-{}", std::process::id()));
        let proj = home.join("Library/Application Support/Some Editor/.claude/projects/-enc");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("s1.jsonl"), b"{}").unwrap();

        let jobs = discover_jobs_in(&home);
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].agent_label, None,
            "host unknown — reported as plain Claude Code"
        );
        std::fs::remove_dir_all(&home).ok();
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
