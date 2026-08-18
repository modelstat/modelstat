//! Scan-job discovery. Walks the known transcript roots and yields one
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
    // started with `--config-dir ~/.claude-instances/second`.
    //
    // Both agents, because the same format under a Desktop host is still a
    // relocated install; the label is decided by artefacts, never here.
    //
    // Each candidate contributes TWO roots, and the depths are the point:
    //
    //   * its PARENT at depth 0, because a candidate names a `.claude`-style
    //     dir exactly, so `<parent>/.claude/projects` is a location and not a
    //     search. Depth 0 is load-bearing — `~/.claude`'s parent is `$HOME`, and
    //     descending five levels from there would walk the user's entire disk
    //     and rediscover, unlabelled, every hosted tree the app-data sweep below
    //     already owns.
    //   * ITSELF at full depth, because a relocated install is a whole copy of
    //     the layout, and a Desktop instance nests its transcripts four levels
    //     down inside its own data dir.
    for agent in ["claude_code", "claude_desktop"] {
        for dir in data_dir_candidates_from(home, agent, process_dirs) {
            let path = PathBuf::from(&dir);
            let label = desktop_host_label(&path);
            if let Some(parent) = path.parent().map(Path::to_path_buf) {
                roots.push((parent, label, 0));
            }
            roots.push((path, label, CLAUDE_SEARCH_MAX_DEPTH));
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
                    } else if f.is_dir() {
                        // A session DIRECTORY beside the session transcript,
                        // holding the conversations of the sub-agents that
                        // session spawned — see `collect_subagents`.
                        collect_subagents(&f.join("subagents"), SUBAGENT_MAX_DEPTH, agent, jobs);
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

/// How far below a session's `subagents/` directory a sub-agent transcript may
/// sit. Two shapes exist on a real machine — `subagents/agent-<id>.jsonl` (353
/// files) and `subagents/workflows/wf_<id>/agent-<id>.jsonl` (1,341) — so the
/// deepest OBSERVED is 2. The bound is one more than that: enough to absorb the
/// next grouping directory Claude Code introduces without a release, and small
/// enough that the walk stays a handful of `read_dir`s per session rather than a
/// disk crawl. Anything deeper is a shape nobody has seen, and inventing depth
/// for it would be guessing at a layout instead of reading one.
const SUBAGENT_MAX_DEPTH: usize = 3;

/// Collect the sub-agent transcripts under one session's `subagents/` tree.
///
/// Each Task/sub-agent a session spawns gets its OWN transcript here, carrying
/// `isSidechain: true` and — the load-bearing part — the PARENT session's
/// `sessionId`, so the parser folds these turns into the session that asked for
/// them without the daemon fabricating anything. They hold real `usage` records:
/// on one developer machine, 1,694 of these files held 7.2 BILLION tokens that
/// no cursor had ever pointed at.
///
/// Only `agent-*.jsonl` is taken. The workflow directories also hold a
/// `journal.jsonl` — the workflow runner's own bookkeeping, not a conversation
/// — and a `.meta.json` sidecar per transcript, which is read by the PARSER as
/// facts about the agent, never walked as a transcript.
fn collect_subagents(
    dir: &Path,
    depth_left: usize,
    agent: Option<&'static str>,
    jobs: &mut Vec<ScanJob>,
) {
    if !dir.is_dir() {
        return;
    }
    for f in child_paths(dir) {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if is_jsonl(&f) && name.starts_with("agent-") {
            jobs.push(ScanJob {
                path: f.to_string_lossy().into_owned(),
                kind: ParserKind::ClaudeCode,
                since_ms: None,
                agent_label: agent.map(str::to_string),
            });
        } else if depth_left > 0 && f.is_dir() && !name.starts_with('.') {
            collect_subagents(&f, depth_left - 1, agent, jobs);
        }
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

    apply_harness_labels_in(home, &mut jobs);
    jobs
}

/// Cursor's chat store, relative to its data directory.
const CURSOR_DB_RELATIVE_PATH: &str = "User/globalStorage/state.vscdb";

/// Discover every scan job under the real `$HOME`.
pub fn discover_jobs() -> Vec<ScanJob> {
    home_dir().map(|h| discover_jobs_in(&h)).unwrap_or_default()
}

/// The session id a job's PATH names, by the owning parser's own derivation.
/// Cursor has none: its store is one global DB, not a per-session file.
fn job_session_id(job: &ScanJob) -> Option<String> {
    match job.kind {
        // A sub-agent transcript is named after the AGENT, so the filename rule
        // finds nothing — the session it belongs to is the directory holding its
        // `subagents/` tree. Without this a BB-driven session would ship its own
        // turns labelled `bb` and its sub-agents' turns labelled `claude_code`,
        // which is one session claiming two tools.
        ParserKind::ClaudeCode => {
            modelstat_parsers::claude_code::derive_session_id_from_filename(&job.path).or_else(
                || modelstat_parsers::claude_code::derive_session_id_from_subagent_path(&job.path),
            )
        }
        ParserKind::Codex => {
            modelstat_parsers::codex::derive_session_id_from_rollout_path(&job.path)
        }
        ParserKind::Pi => modelstat_parsers::pi::derive_session_id_from_pi_path(&job.path),
        ParserKind::Cursor => None,
    }
}

/// The BB session-id set, cached per home. `built_wall_ms` sits beside the
/// monotonic instant so a transcript's mtime (wall clock) can be compared
/// against WHEN the set was read.
struct HarnessCache {
    built: std::time::Instant,
    built_wall_ms: u128,
    ids: std::collections::HashSet<String>,
}

/// Ceiling between BB-store reads: past this age any labelling pass re-reads.
const HARNESS_REFRESH_MAX: std::time::Duration = std::time::Duration::from_secs(30);
/// Floor between BB-store reads when a refresh is DEMANDED — an unlabelled
/// transcript newer than the cached set (a session that may have started after
/// the last read). Keeps an active non-BB session, which stays unknown
/// forever, from forcing a snapshot copy on every scan pass.
const HARNESS_REFRESH_MIN: std::time::Duration = std::time::Duration::from_secs(10);

/// Is the cached set stale enough to re-read the store?
fn harness_refresh_due(age: std::time::Duration, unknown_newer_file: bool) -> bool {
    age >= HARNESS_REFRESH_MAX || (unknown_newer_file && age >= HARNESS_REFRESH_MIN)
}

/// Label jobs whose session BB's store claims (`agent = "bb"`).
///
/// BB (an agentic IDE / agent harness) spawns Claude Code via the Agent SDK
/// with the USER'S own home, so its transcripts sit in `~/.claude/projects`
/// beside plain CLI runs — path-based attribution (the `claude_desktop`
/// route) cannot tell them apart, and the transcript itself only says
/// `entrypoint: sdk-cli`, an SDK embedder with no name. BB's own store can:
/// it records each thread's provider session id, and a job whose path derives
/// to one of those ids was driven by BB. A label set by a more specific claim
/// (a Desktop-hosted tree) is never overwritten, and an id the store does not
/// name stays honestly unlabelled.
///
/// The set is cached (per home) so a scan pass costs no snapshot copy in the
/// steady state; a brand-new session's first parse still sees its label,
/// because an unlabelled transcript NEWER than the cached set forces one
/// bounded re-read (see [`HARNESS_REFRESH_MIN`]).
fn apply_harness_labels_in(home: &Path, jobs: &mut [ScanJob]) {
    let candidates: Vec<(usize, String)> = jobs
        .iter()
        .enumerate()
        .filter(|(_, j)| j.agent_label.is_none())
        .filter_map(|(i, j)| job_session_id(j).map(|id| (i, id)))
        .collect();
    if candidates.is_empty() {
        return;
    }

    type Caches = std::sync::Mutex<std::collections::HashMap<PathBuf, HarnessCache>>;
    static CACHES: std::sync::OnceLock<Caches> = std::sync::OnceLock::new();
    let mut caches = CACHES
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let refresh = match caches.get(home) {
        None => true,
        Some(c) => {
            let unknown_newer_file = candidates
                .iter()
                .any(|(i, id)| !c.ids.contains(id) && mtime_ms(&jobs[*i].path) > c.built_wall_ms);
            harness_refresh_due(c.built.elapsed(), unknown_newer_file)
        }
    };
    if refresh {
        let built_wall_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        caches.insert(
            home.to_path_buf(),
            HarnessCache {
                built: std::time::Instant::now(),
                built_wall_ms,
                ids: modelstat_parsers::harness::bb_session_ids_in(home)
                    .into_iter()
                    .collect(),
            },
        );
    }

    let ids = &caches.get(home).expect("inserted above when absent").ids;
    for (i, id) in candidates {
        if ids.contains(&id) {
            jobs[i].agent_label = Some("bb".to_string());
        }
    }
}

/// [`apply_harness_labels_in`] against the real `$HOME` — for jobs built
/// outside discovery (the eager scan's ad-hoc file target).
pub(crate) fn apply_harness_labels(jobs: &mut [ScanJob]) {
    if let Some(home) = home_dir() {
        apply_harness_labels_in(&home, jobs);
    }
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
    jobs.sort_by_cached_key(|j| std::cmp::Reverse(mtime_ms(&j.path)));
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

    /// The sub-agent transcripts nobody read. Both observed shapes are walked,
    /// the sidecar is never mistaken for a conversation, and the workflow
    /// runner's own `journal.jsonl` — which is not one either — stays out.
    #[test]
    fn a_sessions_sub_agent_transcripts_are_discovered_in_both_observed_shapes() {
        let home = std::env::temp_dir().join(format!("modelstat-sub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mk = |rel: &str| {
            let p = home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"{}").unwrap();
        };
        let sess = ".claude/projects/-enc/aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb";
        mk(".claude/projects/-enc/aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb.jsonl");
        mk(&format!("{sess}/subagents/agent-a1.jsonl"));
        mk(&format!("{sess}/subagents/agent-a1.meta.json"));
        mk(&format!("{sess}/subagents/workflows/wf_5f9/agent-a2.jsonl"));
        mk(&format!("{sess}/subagents/workflows/wf_5f9/journal.jsonl"));

        let jobs = jobs_in(&home);
        let names: Vec<&str> = jobs
            .iter()
            .map(|j| j.path.rsplit(['/', '\\']).next().unwrap())
            .collect();
        assert!(names.contains(&"agent-a1.jsonl"), "{names:?}");
        assert!(
            names.contains(&"agent-a2.jsonl"),
            "the workflows/wf_<id>/ shape is 1,341 of 1,694 real files: {names:?}"
        );
        assert!(
            names.contains(&"aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb.jsonl"),
            "the session's own transcript is still walked: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.ends_with(".meta.json")),
            "the sidecar is facts ABOUT an agent, not a transcript: {names:?}"
        );
        assert!(
            !names.contains(&"journal.jsonl"),
            "the workflow runner's bookkeeping is not a conversation: {names:?}"
        );
        assert!(jobs.iter().all(|j| j.kind == ParserKind::ClaudeCode));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A sub-agent transcript sits deeper than a session transcript, so the
    /// watcher's grandparent rule would land on its own directory — but the
    /// projects root is an ancestor already in the set, and recursive watches
    /// collapse it. Asserted rather than assumed: real-time capture for these
    /// files depends on it.
    #[test]
    fn one_recursive_watch_still_covers_the_sub_agent_trees() {
        let home = Path::new("/home/dev");
        let main = Path::new("/home/dev/.claude/projects/-enc/session.jsonl");
        let sub = Path::new("/home/dev/.claude/projects/-enc/session/subagents/agent-a1.jsonl");
        let nested = Path::new(
            "/home/dev/.claude/projects/-enc/session/subagents/workflows/wf_5f9/agent-a2.jsonl",
        );
        assert_eq!(
            crate::watch::covering_watch_dirs(&[main, sub, nested], home),
            vec![PathBuf::from("/home/dev/.claude/projects")],
        );
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
            // Compared as PATHS: the test writes its tree with `/` and discovery
            // emits the platform's separators.
            assert_eq!(
                PathBuf::from(&cursor[0].path),
                db.components().collect::<PathBuf>()
            );
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

#[cfg(test)]
mod harness_label_tests {
    use super::tests::jobs_in;
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("modelstat-bbl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn mk(home: &Path, rel: &str) {
        let p = home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"{}").unwrap();
    }

    /// A BB store naming exactly these provider session ids.
    fn write_bb_db(home: &Path, ids: &[&str]) {
        let dir = home.join(".bb");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("bb.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE events (
                id TEXT PRIMARY KEY NOT NULL,
                thread_id TEXT NOT NULL,
                provider_thread_id TEXT,
                sequence INTEGER NOT NULL,
                type TEXT NOT NULL,
                data TEXT DEFAULT '{}' NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        for (i, id) in ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO events (id, thread_id, provider_thread_id, sequence, type, data, created_at) \
                 VALUES (?1, 'thr_x', ?2, ?3, 'turn/started', '{}', 0)",
                rusqlite::params![format!("evt_{i}"), id, i as i64],
            )
            .unwrap();
        }
    }

    const BB_UUID: &str = "11111111-1111-4111-8111-111111111111";
    const CLI_UUID: &str = "22222222-2222-4222-8222-222222222222";

    /// The heart of the feature: two Claude Code transcripts in the SAME
    /// `~/.claude/projects` tree — one driven by BB, one a plain CLI run —
    /// distinguishable only through BB's own store.
    #[test]
    fn a_bb_driven_claude_session_is_labelled_and_its_cli_neighbour_is_not() {
        let home = temp_home("claude");
        mk(&home, &format!(".claude/projects/-enc/{BB_UUID}.jsonl"));
        mk(&home, &format!(".claude/projects/-enc/{CLI_UUID}.jsonl"));
        write_bb_db(&home, &[BB_UUID]);

        let jobs = jobs_in(&home);
        let label = |uuid: &str| {
            jobs.iter()
                .find(|j| j.path.contains(uuid))
                .expect("job discovered")
                .agent_label
                .clone()
        };
        assert_eq!(label(BB_UUID).as_deref(), Some("bb"));
        assert_eq!(label(CLI_UUID), None, "a plain CLI run stays Claude Code");
        std::fs::remove_dir_all(&home).ok();
    }

    /// The match goes through each parser's own session-id derivation, so a BB
    /// provider that writes codex/pi-shaped artefacts labels for free.
    #[test]
    fn codex_and_pi_sessions_named_by_bb_label_through_their_own_derivations() {
        let home = temp_home("multi");
        let codex_uuid = "33333333-3333-4333-8333-333333333333";
        let pi_uuid = "44444444-4444-4444-8444-444444444444";
        mk(
            &home,
            &format!(".codex/sessions/2026/07/16/rollout-2026-07-16T12-00-00-{codex_uuid}.jsonl"),
        );
        mk(
            &home,
            &format!(".pi/agent/sessions/p/2026-07-16T12-00-00_{pi_uuid}.jsonl"),
        );
        write_bb_db(&home, &[codex_uuid, pi_uuid]);

        let jobs = jobs_in(&home);
        assert!(jobs
            .iter()
            .any(|j| j.kind == ParserKind::Codex && j.agent_label.as_deref() == Some("bb")));
        assert!(jobs
            .iter()
            .any(|j| j.kind == ParserKind::Pi && j.agent_label.as_deref() == Some("bb")));
        std::fs::remove_dir_all(&home).ok();
    }

    /// No BB on the machine → the labelling pass is a no-op, never an error.
    #[test]
    fn a_home_without_a_bb_store_labels_nothing() {
        let home = temp_home("none");
        mk(&home, &format!(".claude/projects/-enc/{CLI_UUID}.jsonl"));

        let jobs = jobs_in(&home);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].agent_label, None);
        std::fs::remove_dir_all(&home).ok();
    }

    /// The refresh policy in one place: a demanded refresh (unlabelled
    /// transcript newer than the cached set) is honoured past the floor, any
    /// refresh past the ceiling, and nothing before either.
    #[test]
    fn the_refresh_policy_holds_its_floor_and_ceiling() {
        use std::time::Duration;
        assert!(!harness_refresh_due(Duration::from_secs(1), false));
        assert!(!harness_refresh_due(Duration::from_secs(1), true));
        assert!(!harness_refresh_due(Duration::from_secs(15), false));
        assert!(harness_refresh_due(Duration::from_secs(15), true));
        assert!(harness_refresh_due(Duration::from_secs(31), false));
    }

    /// The ad-hoc eager path labels the same way discovery does.
    #[test]
    fn an_ad_hoc_job_earns_its_label_from_the_real_home_store() {
        // Exercised through `apply_harness_labels_in` directly: the pub(crate)
        // wrapper only substitutes the real `$HOME`.
        let home = temp_home("adhoc");
        let rel = format!(".claude/projects/-enc/{BB_UUID}.jsonl");
        mk(&home, &rel);
        write_bb_db(&home, &[BB_UUID]);
        let mut jobs = vec![ScanJob {
            path: home.join(&rel).to_string_lossy().into_owned(),
            kind: ParserKind::ClaudeCode,
            since_ms: None,
            agent_label: None,
        }];
        apply_harness_labels_in(&home, &mut jobs);
        assert_eq!(jobs[0].agent_label.as_deref(), Some("bb"));
        std::fs::remove_dir_all(&home).ok();
    }
}
