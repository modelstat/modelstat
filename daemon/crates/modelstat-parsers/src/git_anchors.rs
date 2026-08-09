//! Pre-AI repo anchors — the ROI denominator, mined from the repo's OWN git
//! history.
//!
//! What an AI-era PR cost is only meaningful next to what this team's PRs used
//! to cost, so the server's effort judge is calibrated PER REPO instead of
//! against a vendor's opaque label set: every PR merged in the window BEFORE
//! the repo went AI-assisted is a labelled example, mined on the customer's own
//! machine from history they can re-read themselves.
//!
//! Privacy: an anchor is public repo SHAPE only — PR number, merge sha, ISO
//! timestamps, integer file/line counts. No subject, no body, no path and no
//! author identity ever reaches an [`AnchorPr`]: the subject is read for its
//! `#<n>` and dropped, `--format` never asks git for a body, and the numstat
//! parse ignores the path column entirely.
//!
//! Best-effort like every other git read here: bounded (`--first-parent`,
//! `--until`, `--max-count`, a 4s per-call timeout, a whole-repo budget) and
//! `None` on any failure — a batch never waits on git, and never fails for it.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use modelstat_wire::{AnchorPr, RepoAnchors};
use regex::Regex;

use crate::git::{run_git, GitResolver};
use crate::git_outcome::{parse_git_log, GitCommit};

/// The default end of the pre-AI window. Copilot was still a preview and no
/// agentic coding tool had shipped, so a PR merged before this is a clean
/// baseline sample. Per-install overridable — a repo that adopted early (or
/// late) is calibrated against its own timeline, not this one.
pub const DEFAULT_CUTOFF: &str = "2022-06-01T00:00:00Z";

/// Anchors kept per repo — the wire cap (`caps::ANCHORS_PER_REPO_COUNT_MAX`),
/// mirrored here so the mine stops producing what the batch would truncate.
pub const DEFAULT_MAX_ANCHORS: usize = 50;

/// Commits walked per repo. First-parent only, so this reaches years back on a
/// mainline while staying a bounded read on a repo with a huge branch fan-in.
const MAX_HISTORY: &str = "2000";

/// Commits read from one PR's branch range.
const MAX_RANGE: &str = "1000";

/// Per-git-call ceiling — the same 4s class as `check_pull_request_outcome`.
const GIT_TIMEOUT: Duration = Duration::from_millis(4_000);

/// Whole-repo ceiling. A mine is ~2 git calls per anchor, so a repo on a
/// stalled network filesystem would otherwise multiply the per-call timeout by
/// a hundred. Past the budget the repo ships the anchors it already has.
const MINE_BUDGET: Duration = Duration::from_secs(20);

// Field/record separators, as in `git_outcome` — bytes that never occur in
// commit text. Re-declared rather than shared because they are that module's
// private detail and the anchor walk asks git for a different field set.
const FS: char = '\u{1f}';
const RS: char = '\u{1e}';

/// The anchor walk's log format: sha, committer date, subject — and NO body.
/// [`parse_git_log`] reads the body as "whatever fields remain", so omitting it
/// costs nothing to parse and keeps commit bodies out of this process entirely.
fn log_format() -> String {
    format!("%H{FS}%cI{FS}%s{RS}")
}

/// Which pre-AI window to mine, and how much of it to keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorConfig {
    /// ISO-8601 instant; only merges STRICTLY before it are anchors.
    pub cutoff: String,
    pub max_anchors: usize,
}

impl Default for AnchorConfig {
    fn default() -> Self {
        Self {
            cutoff: DEFAULT_CUTOFF.to_string(),
            max_anchors: DEFAULT_MAX_ANCHORS,
        }
    }
}

/// The PR number a merge subject names, by GitHub's two STRUCTURAL conventions:
/// a merge commit's `Merge pull request #123 from …`, and a squash/rebase
/// merge's trailing `(#123)`. Pure.
///
/// A prose mention is deliberately not one. `git_outcome` reads a bare `#123`
/// anywhere in a subject as a merge, and is right to: it is checking a PR the
/// session already named. Mining has no such witness — "fix bug reported in
/// #123" would invent a calibration point out of a sentence, and every anchor
/// invented here is a wrong baseline the server then measures real work against.
pub fn pr_number_from_subject(subject: &str) -> Option<u64> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^Merge pull request #([0-9]+)|\(#([0-9]+)\)\s*$").unwrap());
    let caps = RE.captures(subject)?;
    caps.get(1).or_else(|| caps.get(2))?.as_str().parse().ok()
}

/// `(files_changed, lines_added, lines_deleted)` from `git show --numstat`.
/// Pure.
///
/// A numstat row is `<added>\t<deleted>\t<path>`. A binary file's `-\t-` row
/// carries no line signal, so it is skipped whole — the regex simply requires
/// digits. The path column is matched but never captured: an anchor ships
/// counts, and this parse gives it no way to ship anything else.
pub fn parse_numstat_totals(stdout: &str) -> (u32, u64, u64) {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([0-9]+)\t([0-9]+)\t").unwrap());
    let (mut files, mut added, mut deleted) = (0u32, 0u64, 0u64);
    for line in stdout.lines() {
        let Some(caps) = RE.captures(line) else {
            continue;
        };
        files = files.saturating_add(1);
        added += caps[1].parse::<u64>().unwrap_or(0);
        deleted += caps[2].parse::<u64>().unwrap_or(0);
    }
    (files, added, deleted)
}

/// `sha → parent shas`, from `git log --format=%H %P`. Pure.
fn parse_parents(stdout: &str) -> HashMap<&str, Vec<&str>> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next()?;
            Some((sha, fields.collect()))
        })
        .collect()
}

/// The PR branch's commit range (`first-parent..second-parent`) for a TRUE
/// merge commit. Pure.
///
/// `None` for a squash or rebase merge: it has one parent, and the branch it
/// flattened is not in this repo's history at all. There is nothing to measure,
/// so nothing is claimed — a span guessed from the merge commit alone would
/// read as "this PR took zero time", which is the one answer that is certainly
/// wrong.
fn merge_range(parents: &[&str]) -> Option<String> {
    match parents {
        [first, second, ..] => Some(format!("{first}..{second}")),
        _ => None,
    }
}

/// ISO-8601 → epoch millis. `None` when it is not an instant we can place.
fn parse_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts.trim())
        .ok()
        .map(|d| d.timestamp_millis())
}

/// `(span_ms, commit_count)` for one PR, from `git log --format=%cI <range>`:
/// how many commits it carried, and how long its first commit stood before the
/// merge landed. Either half is `None` when the history cannot say it. Pure.
fn span_and_count(range_log: &str, merged_at: &str) -> (Option<u64>, Option<u32>) {
    let lines: Vec<&str> = range_log
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let count = u32::try_from(lines.len()).ok().filter(|n| *n > 0);
    let earliest = lines.iter().copied().filter_map(parse_ms).min();
    let span = match (parse_ms(merged_at), earliest) {
        (Some(end), Some(start)) if end >= start => Some((end - start) as u64),
        _ => None,
    };
    (span, count)
}

/// `ts` strictly before `cutoff`, compared as instants. Pure.
///
/// False when either side is unreadable: an anchor we cannot place in time
/// cannot be shown to be pre-AI, and a maybe-AI-era sample poisons the very
/// baseline it would join.
fn is_before(ts: &str, cutoff: &str) -> bool {
    match (parse_ms(ts), parse_ms(cutoff)) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

/// The commits to anchor on: subjects naming a PR by one of the two structural
/// conventions, merged strictly before the cutoff, newest first, one per PR
/// number, capped at `cfg.max_anchors`. Pure.
///
/// The cap applies AFTER the sort, so a repo with a decade of history anchors
/// on its most recent pre-AI period — the months whose team, stack and review
/// habits most resemble the AI-era work being calibrated.
///
/// The cutoff is re-checked here even though the walk passes `--until`: git
/// compares `--until` against each commit's own timezone, which makes it a
/// cheap walk bound rather than the contract. This is the contract.
pub fn select_anchor_commits<'a>(
    commits: &'a [GitCommit],
    cfg: &AnchorConfig,
) -> Vec<(u64, &'a GitCommit)> {
    let mut hits: Vec<(u64, &GitCommit)> = commits
        .iter()
        .filter(|c| is_before(&c.committed_at, &cfg.cutoff))
        .filter_map(|c| Some((pr_number_from_subject(&c.subject)?, c)))
        .collect();
    hits.sort_by(|a, b| parse_ms(&b.1.committed_at).cmp(&parse_ms(&a.1.committed_at)));
    // A backport or cherry-pick carries the original `(#123)` forward, so one PR
    // can appear twice on a mainline. Counting it twice would weight that PR
    // double in the baseline; the newest occurrence wins.
    let mut seen = HashSet::new();
    hits.retain(|(pr, _)| seen.insert(*pr));
    hits.truncate(cfg.max_anchors);
    hits
}

/// The repo's HEAD sha, or `None` when `cwd` is not a readable git repo. The
/// caller reads this to decide whether a re-mine is warranted at all — HEAD
/// unchanged means the answer cannot have changed.
pub fn head_sha(cwd: &str) -> Option<String> {
    let out = run_git(&["rev-parse", "HEAD"], cwd, GIT_TIMEOUT)?;
    let sha = out.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Mine one repo's pre-AI merged PRs. `None` when `cwd` is not a git repo, has
/// no remote slug to join on, or git fails — never an error, never a block.
///
/// A repo whose pre-AI history holds no PR merges returns `Some` with an empty
/// `anchors`: the mine RAN and found nothing, which is a different fact from
/// "the mine could not run", and the caller caches the former so it stops
/// re-walking a repo that has nothing to say.
pub fn mine_repo_anchors(cwd: &str, cfg: &AnchorConfig) -> Option<RepoAnchors> {
    let ctx = GitResolver::new().resolve(Some(cwd))?;
    // No slug ⇒ no join key ⇒ the server has nothing to attach these to.
    let slug = ctx.remote_slug?;
    let head_sha = head_sha(cwd)?;

    let until = format!("--until={}", cfg.cutoff);
    let format = format!("--format={}", log_format());
    let log = run_git(
        &["log", "--first-parent", &until, "-n", MAX_HISTORY, &format],
        cwd,
        GIT_TIMEOUT,
    )?;
    let commits = parse_git_log(&log);
    let selected = select_anchor_commits(&commits, cfg);
    // Parents in one bulk read over the same bounded walk, rather than a
    // `rev-list` per anchor: it is what tells a true merge from a squash, and
    // fifty extra subprocesses to learn it would cost more than the walk.
    let parents_out = run_git(
        &[
            "log",
            "--first-parent",
            &until,
            "-n",
            MAX_HISTORY,
            "--format=%H %P",
        ],
        cwd,
        GIT_TIMEOUT,
    )?;
    let parents = parse_parents(&parents_out);

    let deadline = Instant::now() + MINE_BUDGET;
    let mut anchors = Vec::with_capacity(selected.len());
    for (pr_number, commit) in selected {
        if Instant::now() >= deadline {
            break;
        }
        // `-m --first-parent`: a merge's default combined diff is empty for a
        // clean merge, which would report every PR as zero lines changed.
        let Some(numstat) = run_git(
            &[
                "-c",
                "core.quotePath=false",
                "show",
                "--numstat",
                "--format=",
                "-m",
                "--first-parent",
                &commit.sha,
            ],
            cwd,
            GIT_TIMEOUT,
        ) else {
            continue;
        };
        let (files_changed, lines_added, lines_deleted) = parse_numstat_totals(&numstat);
        let range = parents
            .get(commit.sha.as_str())
            .and_then(|shas| merge_range(shas));
        let (span_ms, commit_count) = match range {
            None => (None, None),
            Some(range) => {
                let branch = run_git(
                    &["log", "--format=%cI", "-n", MAX_RANGE, &range],
                    cwd,
                    GIT_TIMEOUT,
                );
                branch
                    .map(|out| span_and_count(&out, &commit.committed_at))
                    .unwrap_or((None, None))
            }
        };
        anchors.push(AnchorPr {
            pr_number,
            merge_sha: commit.sha.clone(),
            merged_at: commit.committed_at.clone(),
            files_changed,
            lines_added,
            lines_deleted,
            span_ms,
            commit_count,
        });
    }

    Some(RepoAnchors {
        slug,
        // Only ever what git itself named (`GitResolver` parses it off the
        // remote URL) — never inferred from a path shape.
        host: ctx.remote_host,
        cutoff: cfg.cutoff.clone(),
        mined_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        head_sha,
        anchors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn commit(sha: &str, at: &str, subject: &str) -> GitCommit {
        GitCommit {
            sha: sha.into(),
            committed_at: at.into(),
            subject: subject.into(),
            body: String::new(),
        }
    }

    #[test]
    fn both_github_merge_conventions_are_read_and_prose_is_not() {
        assert_eq!(
            pr_number_from_subject("Merge pull request #42 from acme/feature"),
            Some(42)
        );
        assert_eq!(pr_number_from_subject("fix: thing (#123)"), Some(123));
        // The whole digit run is the number: `(#1234)` is PR 1234, never 123.
        assert_eq!(pr_number_from_subject("chore: bump (#1234)"), Some(1234));
        // A sentence about a PR is not a merge of one.
        assert_eq!(pr_number_from_subject("fix: bug reported in #123"), None);
        assert_eq!(pr_number_from_subject("wip"), None);
    }

    #[test]
    fn numstat_totals_sum_files_and_skip_binaries() {
        let out = "3\t1\tsrc/a.ts\n\
                   -\t-\tassets/logo.png\n\
                   2\t0\tsrc/b.ts\n";
        // The binary row carries no line signal and no countable change.
        assert_eq!(parse_numstat_totals(out), (2, 5, 1));
        assert_eq!(parse_numstat_totals(""), (0, 0, 0));
    }

    #[test]
    fn parents_split_merges_from_squashes() {
        let out = "aaa bbb ccc\nbbb ddd\nddd\n";
        let parents = parse_parents(out);
        assert_eq!(parents["aaa"], vec!["bbb", "ccc"]);
        assert_eq!(merge_range(&parents["aaa"]).as_deref(), Some("bbb..ccc"));
        // A squash merge flattened its branch away — we never invent a span.
        assert_eq!(merge_range(&parents["bbb"]), None);
        assert_eq!(merge_range(&parents["ddd"]), None);
    }

    #[test]
    fn span_is_first_commit_to_merge_and_count_is_the_branch() {
        let range = "2021-03-04T12:00:00Z\n2021-03-02T12:00:00Z\n";
        let (span, count) = span_and_count(range, "2021-03-05T12:00:00Z");
        // Earliest branch commit → merge = 3 days.
        assert_eq!(span, Some(3 * 86_400_000));
        assert_eq!(count, Some(2));
        // An empty range says nothing rather than zero.
        assert_eq!(span_and_count("", "2021-03-05T12:00:00Z"), (None, None));
    }

    #[test]
    fn selection_takes_the_newest_pre_cutoff_merges_up_to_the_cap() {
        let commits = vec![
            // Post-cutoff: AI-era work is the thing being measured, not a baseline.
            commit("post", "2026-01-01T00:00:00Z", "feat: modern (#900)"),
            commit("c3", "2022-05-01T00:00:00Z", "feat: three (#3)"),
            commit("c1", "2021-01-01T00:00:00Z", "feat: one (#1)"),
            commit("c2", "2021-06-01T00:00:00Z", "Merge pull request #2 from a/b"),
            commit("prose", "2021-07-01T00:00:00Z", "chore: see #77 for why"),
        ];
        let cfg = AnchorConfig::default();
        let picked = select_anchor_commits(&commits, &cfg);
        assert_eq!(
            picked.iter().map(|(pr, _)| *pr).collect::<Vec<_>>(),
            vec![3, 2, 1],
            "newest first, post-cutoff and prose excluded"
        );

        // The cap keeps the NEWEST n, not the first n walked.
        let capped = select_anchor_commits(
            &commits,
            &AnchorConfig {
                max_anchors: 2,
                ..cfg
            },
        );
        assert_eq!(
            capped.iter().map(|(pr, _)| *pr).collect::<Vec<_>>(),
            vec![3, 2]
        );
    }

    #[test]
    fn a_backported_pr_number_is_counted_once() {
        let commits = vec![
            commit("new", "2021-09-01T00:00:00Z", "fix: thing (#5)"),
            commit("old", "2021-08-01T00:00:00Z", "fix: thing (#5)"),
        ];
        let picked = select_anchor_commits(&commits, &AnchorConfig::default());
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].1.sha, "new");
    }

    #[test]
    fn an_unplaceable_timestamp_is_never_pre_cutoff() {
        assert!(is_before("2021-01-01T00:00:00Z", DEFAULT_CUTOFF));
        assert!(!is_before("2026-01-01T00:00:00Z", DEFAULT_CUTOFF));
        assert!(!is_before("who knows", DEFAULT_CUTOFF));
        assert!(!is_before("2021-01-01T00:00:00Z", "not a date"));
    }

    /// End-to-end over a real repo: the git invocations themselves are the part
    /// a pure test cannot check, and they are version-sensitive (a merge's
    /// default `git show` diff is EMPTY — `-m --first-parent` is what makes the
    /// line counts real).
    #[test]
    #[cfg(unix)]
    fn mines_a_true_merge_and_excludes_post_cutoff_work() {
        let dir = std::env::temp_dir().join(format!("modelstat-anchors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_str().unwrap().to_string();
        let git = |args: &[&str], at: &str| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_DATE", at)
                .env("GIT_COMMITTER_DATE", at)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        };
        // Skip cleanly if git isn't available on this runner.
        if git(&["init", "-q"], "").map(|s| !s.success()).unwrap_or(true) {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let commit = |message: &str, at: &str| {
            git(
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "-m",
                    message,
                ],
                at,
            )
        };
        let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).unwrap();

        let _ = git(
            &["config", "remote.origin.url", "git@github.com:acme/myrepo.git"],
            "",
        );
        write("a.txt", "1\n");
        let _ = git(&["add", "-A"], "");
        let _ = commit("init", "2021-01-01T00:00:00Z");
        let _ = git(&["branch", "-M", "base"], "");
        let _ = git(&["checkout", "-q", "-b", "feature"], "");
        write("a.txt", "1\n2\n3\n");
        let _ = git(&["add", "-A"], "");
        let _ = commit("wip one", "2021-01-02T00:00:00Z");
        write("b.txt", "x\ny\n");
        let _ = git(&["add", "-A"], "");
        let _ = commit("wip two", "2021-01-03T00:00:00Z");
        let _ = git(&["checkout", "-q", "base"], "");
        let _ = git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "Merge pull request #42 from acme/feature",
                "feature",
            ],
            "2021-01-05T00:00:00Z",
        );
        // AI-era work on the same mainline: never an anchor.
        let _ = commit_empty(&dir, "feat: modern (#900)", "2026-01-01T00:00:00Z");

        let mined = mine_repo_anchors(&path, &AnchorConfig::default()).unwrap();
        assert_eq!(mined.slug, "acme/myrepo");
        assert_eq!(mined.host.as_deref(), Some("github.com"));
        assert_eq!(mined.cutoff, DEFAULT_CUTOFF);
        assert_eq!(mined.head_sha.len(), 40);
        assert_eq!(
            mined.anchors.iter().map(|a| a.pr_number).collect::<Vec<_>>(),
            vec![42],
            "the post-cutoff (#900) merge is not a baseline"
        );
        let anchor = &mined.anchors[0];
        assert_eq!(anchor.files_changed, 2);
        assert_eq!(anchor.lines_added, 4);
        assert_eq!(anchor.lines_deleted, 0);
        // A true merge: its branch is still here, so the span is real.
        assert_eq!(anchor.commit_count, Some(2));
        assert_eq!(anchor.span_ms, Some(3 * 86_400_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn commit_empty(dir: &std::path::Path, message: &str, at: &str) -> std::io::Result<()> {
        Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                message,
            ])
            .current_dir(dir)
            .env("GIT_AUTHOR_DATE", at)
            .env("GIT_COMMITTER_DATE", at)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|_| ())
    }
}
