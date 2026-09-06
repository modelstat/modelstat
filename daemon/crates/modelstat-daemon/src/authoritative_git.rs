//! Authoritative git-remote enrichment. Run over a batch of parsed events BEFORE
//! segmentation so the project identity keys on the REAL repository, not the
//! parser's `guessRepoSlugFromPath` heuristic (which mistakes a repo-internal
//! `…/src/app/x` subtree for a repo).
//!
//! The repository is a fact about the FILES a turn touched, so that is what is
//! resolved. Per event the candidates are, most specific first: the parent
//! directory of every path the turn's tool calls named (`RawEvent::tool_paths`),
//! then `cwd`. `cwd` comes last because it is only where the agent was STARTED,
//! which need not be anywhere near what it edited — an agent launched from a
//! directory that HOLDS many checkouts but is not itself one resolves to no
//! repo at all, and so states none, forever. That is how `pi` reached 0 stated
//! repos over 327 sessions and `cursor` 0 over 202, while `claude_code`,
//! normally launched inside the checkout, reached 1,131 of 1,147. When the cwd
//! IS a repo the ordering costs nothing: that session's tool paths sit inside
//! the same repo and resolve to the same identity.
//!
//! Per candidate directory, resolve in order: (1) the real `owner/repo` from
//! `remote.origin.url`, else (2) the repo-ROOT directory name (a bare name that
//! can never be a subdirectory) for a repo with no remote. The first candidate
//! yielding either wins for that event; when NO candidate reaches a `.git` the
//! parser's own value is left exactly as it was, guess marker and all — never a
//! fabricated slug. The turn's HISTORICAL branch is preserved (env inference +
//! branch-ticket detection want it); only the repo identity is corrected.

use std::collections::HashMap;

use modelstat_wire::{GitContext, RawEvent, SLUG_SOURCE_GIT_REMOTE, SLUG_SOURCE_REPO_ROOT_DIR};

/// How many DISTINCT repositories one batch may resolve through `git`.
///
/// Every unseen directory is a `.git` walk (filesystem stats — cheap), and a
/// walk that lands in a repo is two `git` subprocesses on top — the expensive
/// part, and what this bounds. A batch runs to `BATCH_MAX_EVENTS` events and
/// each of them may name files in a subtree of its own, so an unbounded set
/// turns a single scan sweep into thousands of processes — and `run_git`
/// carries a timeout precisely because git must never hold up a scan.
///
/// Keyed on the resolved ROOT, never on the directory: N subdirectories of
/// one checkout cost one slot. It was a flat count of directories until the
/// v29 re-scan showed what that does to a real batch — a re-scan batches many
/// sessions, every file's parent is its own directory, and 64 of those are
/// gone a few sessions in; every event after that in the batch resolved
/// nothing, whole sessions included (17 sessions that name a checkout on
/// disk shipped with no repo, on one machine, in one re-scan). Of 162 linked
/// sessions measured, 140 touched exactly one repo owner, so 64 roots is far
/// more than a batch needs, and a batch that spends it still answers every
/// remaining event from what it already resolved.
const MAX_CANDIDATE_ROOTS: usize = 64;

/// The corrected repo identity for a directory (slug always present).
struct RepoIdentity {
    remote_url: Option<String>,
    remote_host: Option<String>,
    remote_slug: String,
    branch: Option<String>,
    /// Which of the two corrections produced `remote_slug` — the configured
    /// remote, or the repo-root directory name. Rides to the server so a bare
    /// root name is never mistaken for an `owner/repo` off a real forge.
    slug_source: &'static str,
}

/// The last path component of `path` (`/a/b/repo` → `repo`), or `""`.
fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Correct each event's `git` to the authoritative on-disk repository.
///
/// `resolve_git` reads a directory's git context (cached per directory in the
/// real impl); `resolve_root` walks to the nearest `.git` root. Both are
/// injected so this is unit-testable without a real repo.
pub fn resolve_authoritative_git(
    events: &[RawEvent],
    mut resolve_git: impl FnMut(&str) -> Option<GitContext>,
    resolve_root: impl Fn(&str) -> Option<String>,
) -> Vec<RawEvent> {
    // Per candidate DIRECTORY, first-seen order — and misses are recorded too.
    // Establishing that a directory sits in no repo costs the same walk as
    // establishing that it does, and a batch names the same handful of
    // directories once per turn; without the miss entries every event would
    // re-walk every dead end the batch has already ruled out. The value is the
    // ROOT the directory sits under, and the identity lives once per root.
    let mut dir_root: HashMap<String, Option<String>> = HashMap::new();
    let mut roots: HashMap<String, Option<RepoIdentity>> = HashMap::new();
    let mut out: Vec<RawEvent> = Vec::with_capacity(events.len());

    for e in events {
        let mut winner: Option<String> = None;
        for dir in candidate_paths(e) {
            if !dir_root.contains_key(&dir) {
                // The walk is filesystem stats — always affordable. A directory
                // with no `.git` above it is a miss, remembered as one.
                let root = resolve_root(&dir);
                if let Some(root) = &root {
                    if !roots.contains_key(root) {
                        if roots.len() >= MAX_CANDIDATE_ROOTS {
                            // Budget spent: answer from what is already known
                            // and ask git about nothing new for the rest of
                            // the batch. The directory stays unmemoised so a
                            // later batch, with a fresh budget, can still
                            // resolve it.
                            continue;
                        }
                        let id = resolve_identity(root, &mut resolve_git);
                        roots.insert(root.clone(), id);
                    }
                }
                dir_root.insert(dir.clone(), root);
            }
            let known = dir_root
                .get(&dir)
                .and_then(Option::as_ref)
                .and_then(|r| roots.get(r))
                .is_some_and(Option::is_some);
            if known {
                winner = Some(dir);
                break;
            }
        }
        let Some(id) = winner
            .and_then(|d| dir_root.get(&d))
            .and_then(Option::as_ref)
            .and_then(|r| roots.get(r))
            .and_then(Option::as_ref)
        else {
            // No candidate reached a `.git` — leave the event exactly as parsed.
            out.push(e.clone());
            continue;
        };
        let mut ev = e.clone();
        ev.git = Some(GitContext {
            remote_url: id.remote_url.clone(),
            remote_host: id.remote_host.clone(),
            remote_slug: Some(id.remote_slug.clone()),
            slug_source: Some(id.slug_source.to_string()),
            // Keep the branch the parser recorded for THIS turn; fall back to
            // the on-disk branch only when the event had none.
            branch: e
                .git
                .as_ref()
                .and_then(|g| g.branch.clone())
                .or_else(|| id.branch.clone()),
        });
        out.push(ev);
    }
    out
}

/// The locations that could hold this event's repository, MOST SPECIFIC
/// FIRST: each path the turn's tool calls named, in the order the calls named
/// them, then `cwd`. The PATH, not its parent: the walk up to `.git` starts
/// wherever it is pointed, so a file resolves exactly as its directory would,
/// and a directory that IS a checkout root resolves too — under the parent
/// rule, a turn that only named `~/Documents/goldsky-infra` walked up from
/// `~/Documents` and found nothing.
fn candidate_paths(e: &RawEvent) -> Vec<String> {
    let cwd = e.cwd.as_deref().filter(|c| !c.is_empty());
    let mut paths: Vec<String> = Vec::new();
    for path in &e.tool_paths {
        if let Some(p) = candidate_path(path, cwd) {
            push_unique(&mut paths, p);
        }
    }
    if let Some(cwd) = cwd {
        push_unique(&mut paths, cwd.to_string());
    }
    paths
}

fn push_unique(dirs: &mut Vec<String>, dir: String) {
    if !dir.is_empty() && !dirs.contains(&dir) {
        dirs.push(dir);
    }
}

/// Both separators, on every platform — the same rule the codex parser states
/// for the paths it reads. A transcript can be written on one machine and read
/// on another, so the source's spelling of a separator is not the host's.
const SEPARATORS: [char; 2] = ['/', '\\'];

/// Does `path` name a location from the filesystem ROOT, by SHAPE?
///
/// `Path::is_absolute` answers for the HOST, and that is the wrong question
/// here: on Windows it is false for `/Users/dev/app` — a perfectly absolute
/// path written by the macOS machine whose transcript this is. Asking the host
/// would splice that onto the session's cwd and resolve a directory nobody
/// visited.
fn states_root(path: &str) -> bool {
    path.starts_with(SEPARATORS)
        // A drive prefix (`C:\src`, `c:/src`) — the one absolute shape that
        // does not open with a separator.
        || matches!(path.as_bytes(), [d, b':', rest @ ..]
            if d.is_ascii_alphabetic() && rest.first().is_some_and(|c| SEPARATORS.contains(&(*c as char))))
}

/// A named path, as one location on this filesystem.
///
/// A path the source stated RELATIVE (`core/rust/main.rs`) names a location
/// only against the session's own cwd, so it is joined against it. One that
/// states the root already names one and is taken as it stands. One that
/// opens with `~/` names it from the HOME directory: agents write paths the
/// way people type them, and `~/Documents/edge-api/src/main.rs` joined onto
/// the cwd named `<cwd>/~/Documents/…`, a directory nobody has, so a whole
/// session of work under `~/…` resolved to no repo (observed: 207 tool-path
/// events, 0 resolved). This daemon reads transcripts on the machine that
/// wrote them, so the host's home is the transcript's home.
///
/// String work rather than [`Path`], for the reason [`SEPARATORS`] gives: the
/// join has to produce the same location whichever machine wrote the
/// transcript and whichever one reads it.
fn candidate_path(path: &str, cwd: Option<&str>) -> Option<String> {
    let path = path.trim();
    if let Some(rest) = path.strip_prefix('~').filter(|r| r.starts_with(SEPARATORS)) {
        // `~/x` — this user's home. `~other/x` is somebody else's and is left
        // to the relative arm below, which names nothing real either way.
        let home = home_dir()?;
        return Some(format!("{}{rest}", home.trim_end_matches(SEPARATORS)));
    }
    let (parent, _) = path.rsplit_once(SEPARATORS)?;
    if parent.is_empty() {
        // A bare filename sits in its own cwd, which is already the last
        // candidate — offering it again would just spend a walk twice. So does
        // a root-anchored name (`/main.rs`), which sits in the root itself.
        return None;
    }
    if states_root(path) {
        return Some(path.to_string());
    }
    // A relative path with NO cwd names nothing this process may guess at.
    // Anchoring it to the DAEMON's own working directory would name a location
    // the session never visited, which is worse than admitting there is no
    // answer — so absent cwd stays absent here.
    cwd.map(|cwd| format!("{}/{path}", cwd.trim_end_matches(SEPARATORS)))
}

/// The reading machine's home directory, as the shell would expand `~`.
/// Overridable in tests through `MODELSTAT_HOME_FOR_TESTS` so a fixture can
/// stand in for the home without touching the real one.
fn home_dir() -> Option<String> {
    if let Ok(h) = std::env::var("MODELSTAT_HOME_FOR_TESTS") {
        return Some(h);
    }
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|h| !h.is_empty())
}

/// The repo identity of one repository ROOT (a directory `resolve_root`
/// answered — it holds a `.git`). The two verified tiers and nothing else: a
/// configured remote, else the root directory's name.
fn resolve_identity(
    root: &str,
    resolve_git: &mut impl FnMut(&str) -> Option<GitContext>,
) -> Option<RepoIdentity> {
    let g = resolve_git(root);
    let slug = g
        .as_ref()
        .and_then(|x| x.remote_slug.clone())
        .filter(|s| !s.is_empty());
    if let Some(slug) = slug {
        // Authoritative: a real `owner/repo` remote.
        let g = g.expect("slug came from g");
        return Some(RepoIdentity {
            remote_url: g.remote_url,
            remote_host: g.remote_host,
            remote_slug: slug,
            branch: g.branch,
            slug_source: SLUG_SOURCE_GIT_REMOTE,
        });
    }
    // No remote → key on the repo-root directory NAME (bare, never a subpath).
    let name = basename(root);
    if name.is_empty() {
        return None;
    }
    Some(RepoIdentity {
        remote_url: None,
        remote_host: None,
        remote_slug: name,
        branch: g.and_then(|x| x.branch),
        slug_source: SLUG_SOURCE_REPO_ROOT_DIR,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_wire::SLUG_SOURCE_PATH_SHAPE;

    fn ev(cwd: Option<&str>, branch: Option<&str>) -> RawEvent {
        ev_paths(cwd, branch, &[])
    }

    /// An event that also states the paths its tool calls named.
    fn ev_paths(cwd: Option<&str>, branch: Option<&str>, paths: &[&str]) -> RawEvent {
        RawEvent {
            seq: None,
            started_at: None,
            first_token_at: None,
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            source_event_id: "e".into(),
            ts: "2026-07-16T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s1".into(),
            actor_id: None,
            recipient_actor_id: None,
            turn_index: None,
            parent_event_id: None,
            cwd: cwd.map(Into::into),
            git: branch.map(|b| GitContext {
                remote_url: None,
                remote_host: None,
                remote_slug: Some("guessed/subdir".into()),
                branch: Some(b.into()),
                slug_source: Some(SLUG_SOURCE_PATH_SHAPE.to_string()),
            }),
            tokens: None,
            tokens_unmapped: std::collections::BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            tool_paths: paths.iter().map(|p| p.to_string()).collect(),
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    /// A fake on-disk world: repo ROOT → the `owner/repo` its
    /// `remote.origin.url` states, or None for a repo with no remote.
    type World = &'static [(&'static str, Option<&'static str>)];

    /// `/Users/dev/Documents` holds two checkouts and is not one itself — the
    /// production shape: an agent started there states no repo of its own.
    const DOCUMENTS: World = &[
        ("/Users/dev/Documents/core", Some("modelstat/core")),
        ("/Users/dev/Documents/edge", Some("goldsky/edge")),
    ];

    /// The root a directory belongs to: the LONGEST configured root that
    /// prefixes it, which is the walk-up the real `find_repo_root` performs.
    fn root_of(world: World, dir: &str) -> Option<&'static str> {
        world
            .iter()
            .map(|(root, _)| *root)
            .filter(|root| dir == *root || dir.starts_with(&format!("{root}/")))
            .max_by_key(|root| root.len())
    }

    /// `resolve_git` over `world`, shaped like the real `GitResolver`: a context
    /// either way, all-null when nothing is reachable.
    fn git_of(world: World) -> impl Fn(&str) -> Option<GitContext> {
        move |dir| {
            let root = root_of(world, dir);
            let slug = root.and_then(|r| world.iter().find(|(x, _)| *x == r).and_then(|(_, s)| *s));
            Some(GitContext {
                remote_url: slug.map(|s| format!("git@github.test:{s}.git")),
                remote_host: slug.map(|_| "github.test".to_string()),
                remote_slug: slug.map(str::to_string),
                branch: root.map(|_| "main".to_string()),
                slug_source: slug.map(|_| SLUG_SOURCE_GIT_REMOTE.to_string()),
            })
        }
    }

    fn root_fn(world: World) -> impl Fn(&str) -> Option<String> {
        move |dir| root_of(world, dir).map(str::to_string)
    }

    fn slug_of(ev: &RawEvent) -> Option<&str> {
        ev.git.as_ref()?.remote_slug.as_deref()
    }

    #[test]
    fn a_real_remote_overrides_the_path_guess_and_keeps_the_turn_branch() {
        let events = vec![ev(Some("/repo/src/app"), Some("feature/x"))];
        let out = resolve_authoritative_git(
            &events,
            |_cwd| {
                Some(GitContext {
                    remote_url: Some("git@github.com:acme/api.git".into()),
                    remote_host: Some("github.com".into()),
                    remote_slug: Some("acme/api".into()),
                    branch: Some("main".into()),
                    slug_source: Some(SLUG_SOURCE_GIT_REMOTE.to_string()),
                })
            },
            |_cwd| Some("/repo".into()),
        );
        let g = out[0].git.as_ref().unwrap();
        // The correction re-labels the provenance too: the path guess is gone.
        assert_eq!(g.slug_source.as_deref(), Some(SLUG_SOURCE_GIT_REMOTE));
        assert_eq!(g.remote_slug.as_deref(), Some("acme/api")); // corrected
        assert_eq!(g.branch.as_deref(), Some("feature/x")); // turn branch preserved
    }

    #[test]
    fn no_remote_falls_back_to_the_repo_root_basename() {
        let events = vec![ev(Some("/home/dev/myrepo/src"), None)];
        let out = resolve_authoritative_git(
            &events,
            |_cwd| {
                Some(GitContext {
                    remote_url: None,
                    remote_host: None,
                    remote_slug: None, // no origin
                    branch: Some("dev".into()),
                    slug_source: None,
                })
            },
            |_cwd| Some("/home/dev/myrepo".into()),
        );
        let g = out[0].git.as_ref().unwrap();
        // A directory name is not an `owner/repo` off a forge, and says so.
        assert_eq!(g.slug_source.as_deref(), Some(SLUG_SOURCE_REPO_ROOT_DIR));
        assert!(g.remote_host.is_none());
        assert_eq!(g.remote_slug.as_deref(), Some("myrepo")); // bare root name
        assert_eq!(g.branch.as_deref(), Some("dev")); // on-disk branch (event had none)
    }

    #[test]
    fn no_git_reachable_leaves_the_event_untouched() {
        let events = vec![ev(Some("/tmp/scratch"), Some("wip"))];
        let out = resolve_authoritative_git(&events, |_| None, |_| None);
        // Unchanged: the parser's guessed slug survives — still labeled a guess.
        let g = out[0].git.as_ref().unwrap();
        assert_eq!(g.remote_slug.as_deref(), Some("guessed/subdir"));
        assert_eq!(g.slug_source.as_deref(), Some(SLUG_SOURCE_PATH_SHAPE));
    }

    #[test]
    fn events_without_a_cwd_pass_through() {
        let events = vec![ev(None, None)];
        let out = resolve_authoritative_git(&events, |_| None, |_| None);
        assert!(out[0].git.is_none());
    }

    /// The production failure, in one test: the agent was launched from the
    /// directory that HOLDS the checkouts, so its cwd is in no repo — and the
    /// file it edited says which repo the work belongs to.
    #[test]
    fn a_path_inside_a_repo_beats_a_cwd_that_is_in_none() {
        let events = vec![ev_paths(
            Some("/Users/dev/Documents"),
            None,
            &["/Users/dev/Documents/core/rust/main.rs"],
        )];
        let out = resolve_authoritative_git(&events, git_of(DOCUMENTS), root_fn(DOCUMENTS));
        let g = out[0].git.as_ref().unwrap();
        assert_eq!(g.remote_slug.as_deref(), Some("modelstat/core"));
        // A real remote, so the hint tiering downstream treats it as verified —
        // which is the whole point: `path_shape` states no repo at all.
        assert_eq!(g.slug_source.as_deref(), Some(SLUG_SOURCE_GIT_REMOTE));
    }

    /// A relative path names a directory only against the session's own cwd.
    /// Nothing in `DOCUMENTS` is reachable from the bare `core/rust`, so an
    /// answer at all proves the join happened.
    #[test]
    fn a_relative_path_is_joined_against_the_cwd_before_resolving() {
        let events = vec![ev_paths(
            Some("/Users/dev/Documents"),
            None,
            &["core/rust/main.rs"],
        )];
        let out = resolve_authoritative_git(&events, git_of(DOCUMENTS), root_fn(DOCUMENTS));
        assert_eq!(slug_of(&out[0]), Some("modelstat/core"));
    }

    /// THE HOST IS NOT THE AUTHOR. A transcript is read on whatever machine
    /// happens to run the daemon, so every shape below has to name the same
    /// directory on every platform. `Path` cannot do this job: it answers
    /// `is_absolute` for the host (false for `/Users/dev/app` on Windows, which
    /// would splice a macOS path onto the cwd) and joins with the host's
    /// separator (so the resolver would be handed two spellings of one
    /// directory and cache-miss on both).
    #[test]
    fn a_stated_path_names_one_location_whoever_reads_it() {
        let cwd = Some("/Users/dev/Documents");
        for (path, want) in [
            // Root-stated, either spelling: taken as it stands, cwd ignored.
            ("/Users/dev/other/main.rs", Some("/Users/dev/other/main.rs")),
            (
                "\\Users\\dev\\other\\main.rs",
                Some("\\Users\\dev\\other\\main.rs"),
            ),
            ("C:\\src\\app\\main.rs", Some("C:\\src\\app\\main.rs")),
            ("c:/src/app/main.rs", Some("c:/src/app/main.rs")),
            // Relative, either spelling: joined against the cwd.
            (
                "core/rust/main.rs",
                Some("/Users/dev/Documents/core/rust/main.rs"),
            ),
            (
                "core\\rust\\main.rs",
                Some("/Users/dev/Documents/core\\rust\\main.rs"),
            ),
            // A bare name, and a root-anchored one, add no directory the cwd
            // does not already offer.
            ("main.rs", None),
            ("/main.rs", None),
        ] {
            assert_eq!(candidate_path(path, cwd).as_deref(), want, "{path}");
        }
        // A relative path with no cwd has nothing to be relative TO, and the
        // daemon's own working directory is not an answer.
        assert_eq!(candidate_path("core/rust/main.rs", None), None);
        // Root-stated still answers without a cwd.
        assert_eq!(
            candidate_path("/Users/dev/other/main.rs", None).as_deref(),
            Some("/Users/dev/other/main.rs")
        );
    }

    /// `~/…` names the home directory, the way the shell reads it — NOT a
    /// directory called `~` under the cwd. Before this, a session whose every
    /// tool call spelled its paths `~/Documents/<repo>/…` (the way people type
    /// them, and the way an agent echoing them writes them) resolved no repo
    /// at all, and was routed as if it had touched nothing.
    #[test]
    fn a_tilde_path_names_the_home_directory() {
        std::env::set_var("MODELSTAT_HOME_FOR_TESTS", "/Users/dev");
        {
            let cwd = Some("/Users/dev/Documents");
            for (path, want) in [
                (
                    "~/Documents/edge-api/src/main.rs",
                    Some("/Users/dev/Documents/edge-api/src/main.rs"),
                ),
                // A directory that IS the checkout — named as itself, so the
                // walk starts inside it rather than one level above.
                (
                    "~/Documents/edge-api",
                    Some("/Users/dev/Documents/edge-api"),
                ),
                // Windows spelling of the same statement.
                (
                    "~\\src\\app\\main.rs",
                    Some("/Users/dev\\src\\app\\main.rs"),
                ),
                // Somebody ELSE's home is not this user's, and stays the
                // relative shape it always was — joined on the cwd.
                (
                    "~alice/repo/main.rs",
                    Some("/Users/dev/Documents/~alice/repo/main.rs"),
                ),
                // A bare `~` names no file.
                ("~", None),
            ] {
                assert_eq!(candidate_path(path, cwd).as_deref(), want, "{path}");
            }
            // Home is home whether or not the session states a cwd.
            assert_eq!(
                candidate_path("~/Documents/edge-api/src/main.rs", None).as_deref(),
                Some("/Users/dev/Documents/edge-api/src/main.rs")
            );
        }
        std::env::remove_var("MODELSTAT_HOME_FOR_TESTS");
    }

    /// The no-regression claim, stated as an invariant rather than a snapshot:
    /// for an agent launched INSIDE its checkout the tool paths sit in the same
    /// repo, so they can only agree with what the cwd already said.
    #[test]
    fn a_repo_cwd_resolves_identically_with_and_without_tool_paths() {
        let cwd = Some("/Users/dev/Documents/core/rust");
        let with = vec![ev_paths(
            cwd,
            Some("feature/x"),
            &[
                "crates/wire/src/lib.rs",
                "/Users/dev/Documents/core/README.md",
            ],
        )];
        let without = vec![ev(cwd, Some("feature/x"))];
        let a = resolve_authoritative_git(&with, git_of(DOCUMENTS), root_fn(DOCUMENTS));
        let b = resolve_authoritative_git(&without, git_of(DOCUMENTS), root_fn(DOCUMENTS));
        assert_eq!(
            a[0].git, b[0].git,
            "a healthy agent's identity must not move"
        );
        assert_eq!(slug_of(&a[0]), Some("modelstat/core"));
    }

    /// Paths that reach no repo are not a licence to invent one.
    #[test]
    fn paths_that_reach_no_repo_leave_the_event_untouched() {
        let events = vec![ev_paths(
            Some("/Users/dev/Documents"),
            Some("wip"),
            &["notes/todo.md", "/tmp/scratch/out.log"],
        )];
        let out = resolve_authoritative_git(&events, git_of(&[]), root_fn(&[]));
        let g = out[0].git.as_ref().unwrap();
        assert_eq!(g.remote_slug.as_deref(), Some("guessed/subdir"));
        assert_eq!(g.slug_source.as_deref(), Some(SLUG_SOURCE_PATH_SHAPE));
    }

    /// One batch, two repos. Placement is per event, so a session that worked
    /// across two checkouts states both — the identity is not a property of the
    /// session's cwd, which is the same for both turns here.
    #[test]
    fn two_events_touching_two_repos_state_two_identities() {
        let events = vec![
            ev_paths(Some("/Users/dev/Documents"), None, &["core/rust/main.rs"]),
            ev_paths(Some("/Users/dev/Documents"), None, &["edge/src/router.rs"]),
        ];
        let out = resolve_authoritative_git(&events, git_of(DOCUMENTS), root_fn(DOCUMENTS));
        assert_eq!(slug_of(&out[0]), Some("modelstat/core"));
        assert_eq!(slug_of(&out[1]), Some("goldsky/edge"));
    }

    /// `run_git` must never hold up a scan, so the number of DISTINCT
    /// repositories a batch asks git about is bounded — and a root already
    /// asked is never asked twice.
    #[test]
    fn distinct_git_resolutions_per_batch_are_bounded() {
        // Every event names its own checkout, so every one is a new root.
        let events: Vec<RawEvent> = (0..(MAX_CANDIDATE_ROOTS * 3))
            .map(|i| {
                ev_paths(
                    Some("/Users/dev/Documents"),
                    None,
                    &[&format!("repo{i}/src/a.rs"), &format!("repo{i}/src/b.rs")],
                )
            })
            .collect();
        let asked = std::cell::RefCell::new(std::collections::BTreeSet::new());
        let out = resolve_authoritative_git(
            &events,
            |dir| {
                asked.borrow_mut().insert(dir.to_string());
                None
            },
            |dir| {
                // Every `/Users/dev/Documents/repoN/…` is a checkout at repoN.
                let rest = dir.strip_prefix("/Users/dev/Documents/")?;
                let repo = rest.split('/').next()?;
                Some(format!("/Users/dev/Documents/{repo}"))
            },
        );
        assert_eq!(out.len(), events.len());
        assert!(
            asked.borrow().len() <= MAX_CANDIDATE_ROOTS,
            "asked git about {} distinct roots, ceiling is {MAX_CANDIDATE_ROOTS}",
            asked.borrow().len()
        );
    }

    /// THE FAILURE the root-keyed budget ends: a batch that names hundreds of
    /// distinct subdirectories of a few checkouts must resolve EVERY event and
    /// ask git once per checkout — not spend the budget on directories and go
    /// blind for the rest of the batch. This is what a re-scan batch looks
    /// like: many sessions, every file's parent its own directory.
    #[test]
    fn many_subdirectories_of_one_checkout_cost_one_slot() {
        let per_repo = MAX_CANDIDATE_ROOTS * 2;
        let mut events: Vec<RawEvent> = Vec::new();
        for repo in ["core", "edge"] {
            for i in 0..per_repo {
                events.push(ev_paths(
                    Some("/Users/dev/Documents"),
                    None,
                    &[&format!("{repo}/src/m{i}/lib.rs")],
                ));
            }
        }
        let world: World = &[
            ("/Users/dev/Documents/core", Some("acme/core")),
            ("/Users/dev/Documents/edge", Some("acme/edge")),
        ];
        let asked = std::cell::RefCell::new(std::collections::BTreeSet::new());
        let git = git_of(world);
        let out = resolve_authoritative_git(
            &events,
            |dir| {
                asked.borrow_mut().insert(dir.to_string());
                git(dir)
            },
            root_fn(world),
        );
        // Every event resolved — the last one as surely as the first.
        let (core, edge) = out.split_at(per_repo);
        assert!(core.iter().all(|e| slug_of(e) == Some("acme/core")));
        assert!(edge.iter().all(|e| slug_of(e) == Some("acme/edge")));
        // And git was asked exactly once per checkout, at its root.
        assert_eq!(
            asked.borrow().iter().cloned().collect::<Vec<_>>(),
            ["/Users/dev/Documents/core", "/Users/dev/Documents/edge"]
        );
    }
}

/// The whole chain for a Cursor session, over a REAL checkout and the REAL
/// resolver — the only test that can catch either half breaking.
///
/// Cursor's chat store names no folder, so until `cursor_workspace` read the
/// editor's own workspace index every Cursor event arrived here with `cwd:
/// None`, this pass had nothing to key on, and no Cursor session ever reached
/// the server carrying a repository. The two
/// halves only pay off together, so they are asserted together.
#[cfg(test)]
mod cursor_chain_tests {
    use modelstat_parsers::parse_cursor_tracking_db;
    use modelstat_parsers::types::ParserContext;
    use modelstat_wire::SLUG_SOURCE_GIT_REMOTE;
    use rusqlite::{params, Connection};
    use std::process::{Command, Stdio};

    /// The `file://` URI an editor writes for a local path.
    ///
    /// Not `format!("file://{path}")`: a POSIX path already opens with the
    /// slash that separates authority from path, and a Windows one
    /// (`C:\\src\\api`) carries neither that slash nor forward separators. Pasting
    /// the raw path produced `file://C:\\src\\api` — no local path at all — so
    /// this test asserted the whole chain on every platform but Windows, where
    /// it failed for a reason that was never in the parser.
    fn file_uri(path: &std::path::Path) -> String {
        let p = path.to_string_lossy().replace('\\', "/");
        if p.starts_with('/') {
            format!("file://{p}")
        } else {
            format!("file:///{p}")
        }
    }

    /// Proves [`file_uri`] on BOTH path shapes from any host, so the Windows
    /// arm is verified here rather than only on a Windows runner.
    #[test]
    fn a_local_path_becomes_a_uri_that_reads_back_as_the_same_path() {
        use modelstat_parsers::cursor_workspace::path_from_file_uri;
        for raw in ["C:\\src\\api", "/src/api"] {
            let uri = file_uri(std::path::Path::new(raw));
            assert_eq!(
                path_from_file_uri(&uri).as_deref(),
                Some(raw.replace('\\', "/").as_str()),
                "round trip for {raw:?} via {uri:?}"
            );
        }
    }

    #[test]
    fn a_cursor_conversation_reaches_the_repository_its_folder_points_at() {
        let root =
            std::env::temp_dir().join(format!("modelstat-cursor-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // A real checkout with a real configured remote.
        let checkout = root.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&checkout)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        };
        // Skip cleanly if git isn't available on this runner.
        if git(&["init", "-q"]).map(|s| !s.success()).unwrap_or(true) {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let _ = git(&["config", "remote.origin.url", "git@github.com:acme/api.git"]);

        // A Cursor install whose workspace index puts one conversation in it.
        let user = root.join("User");
        std::fs::create_dir_all(user.join("globalStorage")).unwrap();
        let store = user.join("globalStorage/state.vscdb");
        let c = Connection::open(&store).unwrap();
        c.execute(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO cursorDiskKV VALUES (?,?)",
            params![
                "bubbleId:comp-1:b1",
                r#"{"type":1,"text":"ship the retry","createdAt":"2026-08-20T10:00:00.000Z"}"#
            ],
        )
        .unwrap();
        drop(c);

        let ws = user.join("workspaceStorage/w0");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("workspace.json"),
            format!(r#"{{"folder": "{}"}}"#, file_uri(&checkout)),
        )
        .unwrap();
        let c = Connection::open(ws.join("state.vscdb")).unwrap();
        c.execute(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            params![
                "composer.composerData",
                r#"{"allComposers":[{"composerId":"comp-1"}]}"#
            ],
        )
        .unwrap();
        drop(c);

        let parsed = parse_cursor_tracking_db(&ParserContext::new(
            "dev-1",
            store.to_string_lossy().as_ref(),
        ))
        .unwrap();
        assert_eq!(parsed.events.len(), 1);

        let out = crate::runtime::make_correct_events()(parsed.events);
        let g = out[0].git.as_ref().expect("the folder was probed");
        assert_eq!(g.remote_slug.as_deref(), Some("acme/api"));
        assert_eq!(g.remote_host.as_deref(), Some("github.com"));
        // `git_remote` is the ONLY provenance the server accepts as naming a
        // repository, so this is the assertion that says the session will land
        // with one.
        assert_eq!(g.slug_source.as_deref(), Some(SLUG_SOURCE_GIT_REMOTE));

        let _ = std::fs::remove_dir_all(&root);
    }
}
