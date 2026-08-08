//! Scan-job discovery — a port of `apps/daemon/src/scan.ts`'s `discoverJobs` +
//! `orderJobsNewestFirst`. Walks the known transcript roots and yields one
//! [`ScanJob`] per `.jsonl`, newest-first so a session you just finished uploads
//! within seconds instead of behind a backlog.
//!
//! Deliberately a direct filesystem walk (not the richer `discover()` installer
//! probe): the scan + the self-healing reconcile must reason over the exact same
//! file set. But WHERE to walk comes from the one source registry
//! (`discovery::data_dir_candidates_from`), because a second list drifts: this
//! module used to hard-code `~/.codex` while the registry honoured `CODEX_HOME`,
//! so a relocated codex was reported as installed and never read.
//!
//! Four agents are walked: Claude Code, Codex, pi/omp, and Cursor. Cursor is the
//! odd one — its conversations live in ONE global key/value DB rather than
//! per-session transcript files, so its floor is a timestamp watermark rather
//! than a byte offset (see `ScanJob::since_ms`).

use std::path::{Path, PathBuf};

use modelstat_parsers::discovery::{
    agent_data_dirs_from_processes, application_data_roots, data_dir_candidates_from, SKIP_DIRS,
};
use modelstat_parsers::{
    parse_claude_code_jsonl, parse_claude_code_jsonl_streaming, parse_codex_rollout,
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

/// Where to hunt for Claude Code transcript trees, and the agent label sessions
/// found under each should carry.
///
/// The label matters: a Desktop-hosted session IS Claude Code by format, but
/// the human used **Claude Desktop**, and `agent` names the tool the human
/// used. Merging them would destroy a distinction nothing can recover later;
/// keeping them apart can always be summed at read time.
fn claude_search_roots(
    home: &Path,
    process_dirs: &[(String, String)],
) -> Vec<(PathBuf, Option<&'static str>, usize)> {
    // The CLI's own home.
    // Depth 0 for the homes: `<home>/.claude/projects` is an exact location, and
    // descending from a home that happens to lack `.claude` would walk the
    // user's entire disk — and rediscover, unlabelled, every hosted tree the
    // app-data roots below already own.
    let mut roots: Vec<(PathBuf, Option<&'static str>, usize)> =
        vec![(home.to_path_buf(), None, 0)];
    // Every place this agent's data can live — the known paths, the env vars
    // that relocate them (`CLAUDE_HOME`), and the directory a RUNNING instance
    // names on its command line, which is the only way to reach a second Claude
    // started with `--config-dir ~/.claude-instances/second`. Each points AT a
    // `.claude` dir, so the shape search starts one level up.
    //
    // Both agents, because the same format under a Desktop host is still a
    // relocated install; the label is decided by artefacts below, never here.
    for agent in ["claude_code", "claude_desktop"] {
        for dir in data_dir_candidates_from(home, agent, process_dirs) {
            let path = PathBuf::from(&dir);
            if let Some(parent) = path.parent().map(Path::to_path_buf) {
                let label = desktop_host_label(&path).or_else(|| desktop_host_label(&parent));
                // Searched as deeply as an application's own data dir: a
                // relocated install is a full copy of the layout, so a Desktop
                // instance nests its transcripts exactly as far down as the
                // original.
                roots.push((parent, label, CLAUDE_SEARCH_MAX_DEPTH));
            }
        }
    }

    // Every application's data directory, per platform — NOT a list of app
    // names. An app's data dir is named after the app, and there is no way to
    // know that name in advance: a second install is "Claude Second", a beta is
    // "Claude Dev", a fork is anything at all. So each one is probed for the
    // transcript shape, and whether it counts as a host is decided by what is
    // INSIDE it, never by what it is called.
    for app_data in application_data_roots(home) {
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

/// Discover every scan job under `home`, reading the RUNNING agents for
/// relocated data directories. Test-injectable root; [`discover_jobs`] passes
/// the real home.
pub fn discover_jobs_in(home: &Path) -> Vec<ScanJob> {
    discover_jobs_in_with(home, &agent_data_dirs_from_processes())
}

/// [`discover_jobs_in`] with the running-process reading supplied.
///
/// A process names an ABSOLUTE directory on this machine, which is the whole
/// value of the probe in production and the whole problem in a test: a suite
/// building a tree under a temp root would otherwise also discover whatever the
/// developer happens to have open, and pass or fail on that. Tests pass `&[]`
/// and get a discovery scoped to the tree they built.
pub fn discover_jobs_in_with(home: &Path, process_dirs: &[(String, String)]) -> Vec<ScanJob> {
    let mut jobs = Vec::new();

    // Claude Code — ~/.claude/projects/<encoded-cwd>/<session>.jsonl, plus the
    // same tree wherever an EMBEDDER puts it. Searched by SHAPE rather than by
    // one hard-coded path: Claude Desktop's local agent mode runs Claude Code
    // with a relocated home, so its transcripts sit at
    // `<app-data>/local-agent-mode-sessions/<a>/<b>/local_<c>/.claude/projects/...`
    // and were invisible for as long as only `$HOME` was walked. Anything else
    // hosting Claude Code the same way is picked up for free.
    for (root, agent, depth) in claude_search_roots(home, process_dirs) {
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

    // Codex — <data-dir>/sessions/<y>/<m>/<d>/rollout-*.jsonl.
    for data_dir in data_dir_candidates_from(home, "codex_cli", process_dirs) {
        for y in child_paths(&PathBuf::from(&data_dir).join("sessions")) {
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
    }

    // pi / omp (Oh My Pi) — <data-dir>/sessions/<p>/*.jsonl. OMP is Pi with its
    // home relocated to ~/.omp; identical JSONL format + parser, and both homes
    // are candidates for the one `pi` source. One level deep by design: top-level
    // <TS>_<uuid>.jsonl are the session transcripts, while nested <TS>_<uuid>/
    // dirs (subagent + tool logs) are skipped so a subagent's tokens aren't
    // double-counted against its parent.
    for data_dir in data_dir_candidates_from(home, "pi", process_dirs) {
        for proj in child_paths(&PathBuf::from(&data_dir).join("sessions")) {
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
    // per-session transcripts: `<data-dir>/User/globalStorage/state.vscdb`.
    // Workspace DBs hold no conversations.
    for data_dir in data_dir_candidates_from(home, "cursor", process_dirs) {
        let db = PathBuf::from(&data_dir).join(CURSOR_DB_RELATIVE_PATH);
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

    // One transcript, one job — the candidate lists overlap by design (a
    // relocated home that a running process ALSO names, say), and scanning a
    // file twice would parse and upload it twice every cycle.
    jobs.sort_by(|a, b| a.path.cmp(&b.path));
    jobs.dedup_by(|a, b| a.path == b.path);

    jobs
}

/// Cursor's chat store, relative to its data directory.
const CURSOR_DB_RELATIVE_PATH: &str = "User/globalStorage/state.vscdb";

/// Discover every scan job under the real `$HOME`.
pub fn discover_jobs() -> Vec<ScanJob> {
    home_dir().map(|h| discover_jobs_in(&h)).unwrap_or_default()
}

/// Parse one job (collect mode) with the right parser.
pub fn parse_job(device_id: &str, job: &ScanJob) -> std::io::Result<ParseResult> {
    let ctx = ParserContext::new(device_id, job.path.clone())
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

    /// No agent is running, so discovery sees exactly the tree the test built.
    pub(super) const NO_PROCESSES: &[(String, String)] = &[];

    /// Serialize every test in this module against the process-global env.
    ///
    /// One of them sets `CODEX_HOME`, and a relocation env var names an ABSOLUTE
    /// directory — so while it is set, every other test's discovery finds that
    /// directory too, whatever root it built. The lock is taken by the READERS
    /// as well as the writer for exactly that reason.
    pub(super) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Discovery over a test's own tree, with nothing of this machine in it.
    pub(super) fn jobs_in(home: &Path) -> Vec<ScanJob> {
        let _g = env_lock();
        discover_jobs_in_with(home, NO_PROCESSES)
    }

    /// A RELOCATED install is scanned, not just reported.
    ///
    /// The escape hatch used to be gated to `claude_code`/`claude_desktop`, so a
    /// codex, pi or Cursor running from a directory no path list names was
    /// DETECTED by the discovery probe — it shows up as an install on the
    /// dashboard — and then never read. That is the worst of the two failure
    /// modes: the tool is visibly present and spends nothing.
    #[test]
    fn a_relocated_install_of_any_agent_is_walked_not_merely_detected() {
        let home = std::env::temp_dir().join(format!("modelstat-reloc-{}", std::process::id()));
        let elsewhere =
            std::env::temp_dir().join(format!("modelstat-elsewhere-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&elsewhere);
        let mk = |p: PathBuf| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"{}").unwrap();
        };
        // Three agents, each living somewhere no known path points at, each
        // named only by the command line of the process running it.
        mk(elsewhere.join("codex-alt/sessions/2026/07/16/rollout-r.jsonl"));
        mk(elsewhere.join("pi-alt/sessions/p/r.jsonl"));
        mk(elsewhere.join("cursor-alt/User/globalStorage/state.vscdb"));
        let processes: Vec<(String, String)> = [
            ("codex_cli", "codex-alt"),
            ("pi", "pi-alt"),
            ("cursor", "cursor-alt"),
        ]
        .iter()
        .map(|(a, d)| {
            (
                (*a).to_string(),
                elsewhere.join(d).to_string_lossy().into_owned(),
            )
        })
        .collect();

        let jobs = {
            let _g = env_lock();
            discover_jobs_in_with(&home, &processes)
        };
        for kind in [ParserKind::Codex, ParserKind::Pi, ParserKind::Cursor] {
            assert_eq!(
                jobs.iter().filter(|j| j.kind == kind).count(),
                1,
                "{kind:?} relocated outside every known path must still be scanned"
            );
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// The env vars that relocate a home are honoured where the SCAN looks, not
    /// only where the installer probe looks. They used to be honoured in exactly
    /// one of the two places.
    #[test]
    fn the_relocation_env_vars_reach_the_scan() {
        let home = std::env::temp_dir().join(format!("modelstat-envreloc-{}", std::process::id()));
        let codex_home = home.join("relocated-codex");
        let _ = std::fs::remove_dir_all(&home);
        let f = codex_home.join("sessions/2026/07/16/rollout-e.jsonl");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, b"{}").unwrap();

        // Held across the set/scan/unset, or a concurrent test discovers the
        // relocated tree this one just pointed the env at.
        let _g = env_lock();
        std::env::set_var("CODEX_HOME", &codex_home);
        let jobs = discover_jobs_in_with(&home, NO_PROCESSES);
        std::env::remove_var("CODEX_HOME");

        assert_eq!(
            jobs.iter().filter(|j| j.kind == ParserKind::Codex).count(),
            1,
            "CODEX_HOME is in the source registry — the scan must read the same registry"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

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

        let jobs = jobs_in(&home);
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
        assert!(jobs_in(&home).is_empty());
    }
}

#[cfg(test)]
mod cursor_discovery_tests {
    use super::tests::jobs_in;
    use super::*;

    /// Cursor's chat store is discovered where it actually lives, on every
    /// platform's user-data path. It went unwalked for the parser's whole
    /// life — the docs claimed an env flag gated it, and no such flag existed.
    #[test]
    fn cursor_global_storage_is_discovered_on_each_platform_layout() {
        // The three platform layouts, now derived from the ONE source registry
        // rather than a second list that could drift from it.
        for app_data in ["Library/Application Support", ".config", "AppData/Roaming"] {
            let rel = format!("{app_data}/Cursor/{CURSOR_DB_RELATIVE_PATH}");
            let home = std::env::temp_dir().join(format!(
                "modelstat-disco-{}-{}",
                std::process::id(),
                rel.replace(['/', ' '], "_")
            ));
            let db = home.join(&rel);
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            std::fs::write(&db, b"not-a-real-db").unwrap();

            let jobs = jobs_in(&home);
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

        let jobs = jobs_in(&home);
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

        let jobs = jobs_in(&home);
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
        assert!(!jobs_in(&home).iter().any(|j| j.kind == ParserKind::Cursor));
        std::fs::remove_dir_all(&home).ok();
    }
}
