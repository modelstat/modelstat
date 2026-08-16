//! Repo anchors — the human-authored baseline, mined from the repo's OWN git
//! history.
//!
//! An anchor is a merged PR that carries NO AI trailer on any of its commits: a
//! human-authored sample from the same repo, the same team and usually the same
//! month as the AI-assisted work beside it. Anchors exist to answer ONE
//! question — which shipped work was AI-assisted and which was not — and to give
//! that split a per-repo denominator.
//!
//! They are deliberately NOT an effort baseline. There is no judge, no
//! calibration and no score anywhere downstream: we report measured primitives
//! (tokens, time, files, lines, lifecycle) and never blend them into a verdict.
//! Git timing was measured against change size at Spearman rho 0.11-0.24, so it
//! cannot support an effort estimate and nothing here pretends otherwise.
//!
//! Why not a date? The first cut of this module anchored on every PR merged
//! before a fixed "pre-AI" instant (2022-06-01). Every repo we ran it against
//! was CREATED after that date, so every repo mined zero anchors and the whole
//! denominator was empty. Adoption is not a calendar fact, it is a per-commit
//! fact, and git already records it — a `Co-Authored-By: Claude` or
//! `Generated with …` trailer is written by the tool itself. A date filter is
//! still available ([`AnchorConfig::cutoff`]) for an operator who wants one; it
//! is off by default, and the AI-vs-human split falls out as a by-product.
//!
//! Privacy: an anchor is public repo SHAPE only — PR number, merge sha, ISO
//! timestamps, integer file/line counts and the numbers derived from them.
//! Subjects and bodies ARE read locally, because the trailer lives in them, but
//! they never leave this function: [`is_ai_authored`] reduces a message to one
//! bool, the subject is read for its `#<n>` and dropped, and the numstat parse
//! ignores the path column entirely. Neither [`AnchorPr`] nor [`RepoAnchors`]
//! has a field a message, an author identity or a path could be stored in.
//!
//! Best-effort like every other git read here: bounded (`--first-parent`,
//! `--max-count`, a 4s per-call timeout, a whole-repo budget) and `None` on any
//! failure — a batch never waits on git, and never fails for it.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use modelstat_wire::{AnchorPr, RepoAnchors};
use regex::Regex;

use crate::git::{run_git, GitResolver};
use crate::git_outcome::{parse_git_log, GitCommit};

/// Anchors kept per repo — the wire cap (`caps::ANCHORS_PER_REPO_COUNT_MAX`),
/// mirrored here so the mine stops producing what the batch would truncate.
pub const DEFAULT_MAX_ANCHORS: usize = 50;

/// The coding tools whose name in a commit trailer means the commit was not
/// written by a human alone. One definition, shared by both trailer forms and
/// by the trailer-KEY form below.
const AI_VENDORS: &str = "claude|codex|cursor|copilot|devin|aider";

/// Silence longer than this between two commits ends a work session: the author
/// stopped, went to a meeting, went to bed. 90 minutes is long enough to cover
/// a lunch inside one sitting and short enough that an overnight review wait
/// never reads as work.
const SESSION_GAP_MS: i64 = 90 * 60 * 1000;

/// Credited to the START of every session. A session's first commit is the
/// output of work that began before it — code written, run and reviewed — and
/// a pure last-minus-first span would price that at zero.
const SESSION_RAMP_MINUTES: i64 = 30;

/// Commits walked per repo. First-parent only, so this reaches years back on a
/// mainline while staying a bounded read on a repo with a huge branch fan-in.
const MAX_HISTORY: &str = "2000";

/// Commits read from one PR's branch range.
const MAX_RANGE: &str = "1000";

/// Per-git-call ceiling — the same 4s class as `check_pull_request_outcome`.
const GIT_TIMEOUT: Duration = Duration::from_millis(4_000);

/// Whole-repo ceiling. A mine is one git call per PR candidate examined and a
/// second for each one kept, so a repo on a stalled network filesystem would
/// otherwise multiply the per-call timeout by a few hundred. Past the budget
/// the repo ships the anchors it already has.
const MINE_BUDGET: Duration = Duration::from_secs(20);

// Field/record separators, as in `git_outcome` — bytes that never occur in
// commit text. Re-declared rather than shared because they are that module's
// private detail, even though the anchor walk now asks for the same field set.
const FS: char = '\u{1f}';
const RS: char = '\u{1e}';

/// The anchor walk's log format: sha, committer date, subject, body. The body
/// is here for exactly one reason — the AI trailer is in it — and is reduced to
/// a bool by [`is_ai_authored`] before anything is built from the commit.
fn log_format() -> String {
    format!("%H{FS}%cI{FS}%s{FS}%b{RS}")
}

/// Which history to mine, and how much of it to keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorConfig {
    /// Optional ISO-8601 instant; when set, only merges STRICTLY before it are
    /// candidates. `None` — the default — mines all history the walk reaches.
    ///
    /// This used to be a required "pre-AI" date and was the reason the miner
    /// returned nothing on every real repo. It survives as an operator escape
    /// hatch (`MODELSTAT_ANCHOR_CUTOFF`), not as the selection rule.
    pub cutoff: Option<String>,
    /// How many human-authored PRs to keep, newest first.
    pub max_anchors: usize,
}

impl Default for AnchorConfig {
    fn default() -> Self {
        Self {
            cutoff: None,
            max_anchors: DEFAULT_MAX_ANCHORS,
        }
    }
}

/// Whether a commit message was written with an AI coding tool, by the trailers
/// those tools write themselves. Case-insensitive, line-scoped, pure.
///
/// THE definition — every count, filter and ratio downstream resolves here, so
/// widening it is a one-line change in one place. Three forms, all real:
///
///   * `Co-Authored-By: Claude <noreply@anthropic.com>` — GitHub's attribution
///     trailer, what Claude Code, Cursor and the Copilot agent all emit today.
///   * `🤖 Generated with [Claude Code](…)` — a sign-off line rather than a
///     trailer, so the marker is matched anywhere on its line.
///   * `Claude-Code: 2.1.0` — a vendor's own hyphenated trailer key.
///
/// The vendor name must sit AFTER the marker on the SAME line, at a word
/// boundary. That is what keeps a human co-author whose commit body happens to
/// mention Codex, or a colleague named in prose, out of the AI bucket — and
/// every false positive here silently shrinks the human baseline.
pub fn is_ai_authored(subject: &str, body: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        // The vendor must follow the marker on the same line; `[^\n]*` is what
        // keeps the scan inside one trailer.
        let signoff =
            format!(r"[^\n]*(?:co-authored-by:|generated with)[^\n]*\b(?:{AI_VENDORS})\b");
        let trailer_key = format!(r"(?:{AI_VENDORS})-[a-z0-9._-]*:");
        Regex::new(&format!(r"(?im)^(?:{signoff}|{trailer_key})")).unwrap()
    });
    RE.is_match(subject) || RE.is_match(body)
}

/// Minutes of ACTIVE work behind one PR, from the epoch-millis timestamps of
/// its own commits. `None` below two timestamps. Pure.
///
/// First-commit-to-merge wall time is not effort: most of a PR's life is it
/// sitting in review overnight, and pricing that as work makes every slow
/// reviewer look like a hard problem. Commits cluster instead — a gap longer
/// than [`SESSION_GAP_MS`] means the author left — so each cluster is one
/// sitting, its span is time actually spent, and each sitting carries a
/// [`SESSION_RAMP_MINUTES`] ramp for the work that produced its first commit.
///
/// This is a proxy and reads as a floor: it cannot see thinking that left no
/// commit behind. It is computed from git alone, on-device, and is the same
/// measurement for a human PR and an AI one — which is all the ratio needs.
pub fn active_minutes(timestamps: &[i64]) -> Option<u32> {
    if timestamps.len() < 2 {
        return None;
    }
    let mut ts = timestamps.to_vec();
    ts.sort_unstable();
    let (mut minutes, mut sessions, mut start) = (0i64, 1i64, ts[0]);
    for pair in ts.windows(2) {
        if pair[1] - pair[0] > SESSION_GAP_MS {
            minutes += (pair[0] - start) / 60_000;
            sessions += 1;
            start = pair[1];
        }
    }
    minutes += (ts[ts.len() - 1] - start) / 60_000;
    u32::try_from(minutes + sessions * SESSION_RAMP_MINUTES).ok()
}

/// The PR number a merge subject names, by GitHub's two STRUCTURAL conventions:
/// a merge commit's `Merge pull request #123 from …`, and a squash/rebase
/// merge's trailing `(#123)`. Pure.
///
/// A prose mention is deliberately not one. `git_outcome` reads a bare `#123`
/// anywhere in a subject as a merge, and is right to: it is checking a PR the
/// session already named. Mining has no such witness — "fix bug reported in
/// #123" would invent an anchor out of a sentence, and a PR misfiled as
/// human-authored corrupts the AI-vs-human split for the whole repo.
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

/// `(span_ms, commit_count)` for one PR from the commits on its branch: how
/// many it carried, and how long its first commit stood before the merge
/// landed. Either half is `None` when the history cannot say it. Pure.
///
/// The span is wall time and stays on the wire as wall time — it is a real fact
/// about the PR (review latency included). [`active_minutes`] is the effort
/// half of the pair; neither substitutes for the other.
fn span_and_count(branch: &[GitCommit], merged_at: &str) -> (Option<u64>, Option<u32>) {
    let count = u32::try_from(branch.len()).ok().filter(|n| *n > 0);
    let earliest = branch
        .iter()
        .filter_map(|c| parse_ms(&c.committed_at))
        .min();
    let span = match (parse_ms(merged_at), earliest) {
        (Some(end), Some(start)) if end >= start => Some((end - start) as u64),
        _ => None,
    };
    (span, count)
}

/// `ts` strictly before `cutoff`, compared as instants. Pure.
///
/// False when either side is unreadable: a commit we cannot place in time
/// cannot be shown to be inside an operator's window, and the safe answer for a
/// filter nobody can evaluate is to filter it out.
fn is_before(ts: &str, cutoff: &str) -> bool {
    match (parse_ms(ts), parse_ms(cutoff)) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

/// The merged-PR CANDIDATES to classify: subjects naming a PR by one of the two
/// structural conventions, newest first, one per PR number, and inside
/// `cfg.cutoff` when an operator set one. Pure.
///
/// Deliberately NOT truncated to `cfg.max_anchors`. Whether a candidate is an
/// anchor is not knowable from its subject — only from the trailers on its
/// commits — so capping here would cap the newest merges and then discard the
/// AI ones among them, which on an AI-heavy repo yields a handful of anchors or
/// none. [`mine_repo_anchors`] applies the cap to the HUMAN PRs it confirms.
pub fn select_anchor_commits<'a>(
    commits: &'a [GitCommit],
    cfg: &AnchorConfig,
) -> Vec<(u64, &'a GitCommit)> {
    let mut hits: Vec<(u64, &GitCommit)> = commits
        .iter()
        .filter(|c| match &cfg.cutoff {
            Some(cutoff) => is_before(&c.committed_at, cutoff),
            None => true,
        })
        .filter_map(|c| Some((pr_number_from_subject(&c.subject)?, c)))
        .collect();
    hits.sort_by_cached_key(|(_, c)| std::cmp::Reverse(parse_ms(&c.committed_at)));
    // A backport or cherry-pick carries the original `(#123)` forward, so one PR
    // can appear twice on a mainline. Counting it twice would weight that PR
    // double in the baseline; the newest occurrence wins.
    let mut seen = HashSet::new();
    hits.retain(|(pr, _)| seen.insert(*pr));
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

/// Mine one repo's human-authored merged PRs. `None` when `cwd` is not a git
/// repo, has no remote slug to join on, or git fails — never an error, never a
/// block.
///
/// Candidates are walked newest-first and each is classified by the trailers on
/// its own commits; AI-assisted ones are counted into `ai_pr_count` and dropped,
/// human ones become anchors until `cfg.max_anchors` is reached. So the two
/// counts describe the SAME recent window — a consumer reading
/// `human_anchor_count: 6, ai_pr_count: 180` knows the baseline is too thin to
/// trust, which an anchor list alone could never tell it.
///
/// A repo whose history holds no human PR merges returns `Some` with an empty
/// `anchors`: the mine RAN and found nothing, which is a different fact from
/// "the mine could not run", and the caller caches the former so it stops
/// re-walking a repo that has nothing to say.
pub fn mine_repo_anchors(cwd: &str, cfg: &AnchorConfig) -> Option<RepoAnchors> {
    let ctx = GitResolver::new().resolve(Some(cwd))?;
    // No slug ⇒ no join key ⇒ the server has nothing to attach these to.
    let slug = ctx.remote_slug?;
    let head_sha = head_sha(cwd)?;

    let format = format!("--format={}", log_format());
    let until = cfg.cutoff.as_ref().map(|c| format!("--until={c}"));
    let mut walk: Vec<&str> = vec!["log", "--first-parent", "-n", MAX_HISTORY, &format];
    if let Some(until) = &until {
        walk.push(until);
    }
    let log = run_git(&walk, cwd, GIT_TIMEOUT)?;
    let commits = parse_git_log(&log);
    // Parents in one bulk read over the same bounded walk, rather than a
    // `rev-list` per candidate: it is what tells a true merge from a squash, and
    // hundreds of extra subprocesses to learn it would cost more than the walk.
    let mut parents_walk: Vec<&str> =
        vec!["log", "--first-parent", "-n", MAX_HISTORY, "--format=%H %P"];
    if let Some(until) = &until {
        parents_walk.push(until);
    }
    let parents_out = run_git(&parents_walk, cwd, GIT_TIMEOUT)?;
    let parents = parse_parents(&parents_out);

    let deadline = Instant::now() + MINE_BUDGET;
    let mut anchors: Vec<AnchorPr> = Vec::with_capacity(cfg.max_anchors);
    let mut ai_pr_count: u32 = 0;
    for (pr_number, commit) in select_anchor_commits(&commits, cfg) {
        if anchors.len() >= cfg.max_anchors || Instant::now() >= deadline {
            break;
        }
        // The branch a TRUE merge brought in. Empty for a squash or rebase
        // merge, which has no branch left to read — and is also where the forge
        // put the squashed commits' trailers, so the merge commit still answers
        // the AI question on its own.
        let branch = parents
            .get(commit.sha.as_str())
            .and_then(|shas| merge_range(shas))
            .and_then(|range| run_git(&["log", &format, "-n", MAX_RANGE, &range], cwd, GIT_TIMEOUT))
            .map(|out| parse_git_log(&out))
            .unwrap_or_default();

        // One AI commit makes the PR AI-assisted: the baseline is asking what a
        // PR costs WITHOUT the tool, and a branch that used it once did not.
        if is_ai_authored(&commit.subject, &commit.body)
            || branch.iter().any(|c| is_ai_authored(&c.subject, &c.body))
        {
            ai_pr_count = ai_pr_count.saturating_add(1);
            continue;
        }

        // `-m --first-parent`: a merge's default combined diff is empty for a
        // clean merge, which would report every PR as zero lines changed. Run
        // only for a confirmed anchor — the AI ones never pay for it.
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
        let (span_ms, commit_count) = span_and_count(&branch, &commit.committed_at);
        // The PR's OWN commits, not the merge: the merge commit is the moment a
        // reviewer pressed a button, and folding it in would charge the author
        // for the wait.
        let stamps: Vec<i64> = branch
            .iter()
            .filter_map(|c| parse_ms(&c.committed_at))
            .collect();
        anchors.push(AnchorPr {
            pr_number,
            merge_sha: commit.sha.clone(),
            merged_at: commit.committed_at.clone(),
            files_changed,
            lines_added,
            lines_deleted,
            span_ms,
            commit_count,
            active_minutes: active_minutes(&stamps),
            ai_assisted: false,
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
        human_anchor_count: u32::try_from(anchors.len()).unwrap_or(u32::MAX),
        ai_pr_count,
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

    const MIN: i64 = 60_000;

    #[test]
    fn every_vendor_trailer_form_is_ai_and_a_human_commit_is_not() {
        // The exact bytes six tools write. Verified against real history in
        // ~/Documents/erpc, where these account for 243 of ~525 merged PRs.
        for trailer in [
            "Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>",
            "Co-authored-by: OpenAI Codex <codex@openai.com>",
            "Co-authored-by: Cursor <cursoragent@cursor.com>",
            "Co-authored-by: copilot-swe-agent[bot] <1@users.noreply.github.com>",
            "Co-authored-by: Devin AI <devin-ai-integration[bot]@users.noreply.github.com>",
            "Co-authored-by: aider (gpt-4o) <aider@aider.chat>",
        ] {
            assert!(
                is_ai_authored("fix: thing (#1)", &format!("why\n\n{trailer}\n")),
                "{trailer} must read as AI-assisted"
            );
        }
        // The sign-off form, emoji and markdown link included.
        assert!(is_ai_authored(
            "fix: thing (#1)",
            "why\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)\n"
        ));
        // A vendor's own hyphenated trailer key.
        assert!(is_ai_authored("fix (#1)", "Claude-Code: 2.1.0\n"));
        assert!(is_ai_authored("fix (#1)", "Codex-Version: 0.9\n"));
        // Detection is case-insensitive on the marker as well as the name.
        assert!(is_ai_authored("fix (#1)", "CO-AUTHORED-BY: CLAUDE <x@y>\n"));
        // …and reads the subject too, for a tool that signs there.
        assert!(is_ai_authored("chore: generated with Codex (#1)", ""));

        // A human PR, co-authors and all.
        assert!(!is_ai_authored(
            "feat: rate limiter (#12)",
            "Adds a token bucket.\n\nCo-authored-by: Kasra <kasra@goldsky.com>\n"
        ));
        assert!(!is_ai_authored("Merge pull request #7 from acme/fix", ""));
    }

    #[test]
    fn prose_naming_a_tool_is_not_an_attribution() {
        // A real erpc commit body: AGENTS.md docs that NAME the tools. The
        // vendor must follow the marker on the SAME line, or every repo that
        // documents its agent setup loses its whole human baseline.
        assert!(!is_ai_authored(
            "docs: agent guide (#3)",
            "Read by Maestro, Codex, Cursor, and reviewers.\n"
        ));
        // Marker present, but the vendor name is BEFORE it and the co-author is
        // a person.
        assert!(!is_ai_authored(
            "fix: cursor jump (#4)",
            "Cursor position was off.\nCo-authored-by: Bob <bob@acme.io>\n"
        ));
        // A bare word plus a colon is prose, not a trailer key.
        assert!(!is_ai_authored("fix (#5)", "cursor: see the note above\n"));
        // Word boundaries: no vendor hides inside another word.
        assert!(!is_ai_authored(
            "fix (#6)",
            "Co-authored-by: Ada Raider <ada@cursory.dev>\n"
        ));
    }

    #[test]
    fn active_minutes_clusters_commits_into_sittings() {
        // One commit is an instant, not a duration.
        assert_eq!(active_minutes(&[]), None);
        assert_eq!(active_minutes(&[1_700_000_000_000]), None);

        // A tight burst is one sitting: its span plus the ramp for the work
        // that produced the first commit.
        let burst = [0, 20 * MIN, 40 * MIN];
        assert_eq!(active_minutes(&burst), Some(40 + 30));

        // Two sittings a working day apart: each span, each ramp — and NOT the
        // eight idle hours between them.
        let two_days = [0, 10 * MIN, 8 * 60 * MIN, 8 * 60 * MIN + 20 * MIN];
        assert_eq!(active_minutes(&two_days), Some(10 + 20 + 60));

        // The gap is strict: exactly 90 minutes is still one sitting, a minute
        // more is two (each with a zero span — only the two ramps count).
        assert_eq!(active_minutes(&[0, 90 * MIN]), Some(90 + 30));
        assert_eq!(active_minutes(&[0, 91 * MIN]), Some(60));

        // Input order is not the caller's problem — `git log` is newest-first.
        assert_eq!(active_minutes(&[40 * MIN, 0, 20 * MIN]), Some(70));
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
        let branch = vec![
            commit("b2", "2021-03-04T12:00:00Z", "wip two"),
            commit("b1", "2021-03-02T12:00:00Z", "wip one"),
        ];
        let (span, count) = span_and_count(&branch, "2021-03-05T12:00:00Z");
        // Earliest branch commit → merge = 3 days.
        assert_eq!(span, Some(3 * 86_400_000));
        assert_eq!(count, Some(2));
        // A squash merge has no branch, and says so rather than saying zero.
        assert_eq!(span_and_count(&[], "2021-03-05T12:00:00Z"), (None, None));
    }

    #[test]
    fn selection_takes_every_pr_merge_newest_first_by_default() {
        let commits = vec![
            commit("c4", "2026-01-01T00:00:00Z", "feat: modern (#900)"),
            commit("c3", "2022-05-01T00:00:00Z", "feat: three (#3)"),
            commit("c1", "2021-01-01T00:00:00Z", "feat: one (#1)"),
            commit(
                "c2",
                "2021-06-01T00:00:00Z",
                "Merge pull request #2 from a/b",
            ),
            commit("prose", "2021-07-01T00:00:00Z", "chore: see #77 for why"),
        ];
        // No cutoff by default: a 2026 merge is a candidate like any other —
        // the AI trailer, not the calendar, decides whether it is an anchor.
        let picked = select_anchor_commits(&commits, &AnchorConfig::default());
        assert_eq!(
            picked.iter().map(|(pr, _)| *pr).collect::<Vec<_>>(),
            vec![900, 3, 2, 1],
            "newest first, prose excluded, nothing dropped for being recent"
        );
        // `max_anchors` does NOT truncate candidates: the cap belongs to the
        // human PRs the mine confirms, not to the merges it must look at.
        let capped = select_anchor_commits(
            &commits,
            &AnchorConfig {
                max_anchors: 2,
                ..Default::default()
            },
        );
        assert_eq!(capped.len(), 4);
    }

    #[test]
    fn an_operator_cutoff_still_bounds_the_window() {
        let commits = vec![
            commit("new", "2026-01-01T00:00:00Z", "feat: modern (#900)"),
            commit("old", "2021-01-01T00:00:00Z", "feat: one (#1)"),
            commit("bad", "who knows", "feat: undated (#2)"),
        ];
        let cfg = AnchorConfig {
            cutoff: Some("2022-06-01T00:00:00Z".into()),
            ..Default::default()
        };
        assert_eq!(
            select_anchor_commits(&commits, &cfg)
                .iter()
                .map(|(pr, _)| *pr)
                .collect::<Vec<_>>(),
            vec![1],
            "an unplaceable timestamp cannot be shown to be inside the window"
        );
        assert!(is_before("2021-01-01T00:00:00Z", "2022-06-01T00:00:00Z"));
        assert!(!is_before("2026-01-01T00:00:00Z", "2022-06-01T00:00:00Z"));
        assert!(!is_before("who knows", "2022-06-01T00:00:00Z"));
        assert!(!is_before("2021-01-01T00:00:00Z", "not a date"));
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

    /// End-to-end over a real repo: the git invocations themselves are the part
    /// a pure test cannot check, and they are version-sensitive (a merge's
    /// default `git show` diff is EMPTY — `-m --first-parent` is what makes the
    /// line counts real). This is the regression that matters: a repo whose
    /// history is entirely post-2022 must still produce anchors.
    #[test]
    #[cfg(unix)]
    fn mines_human_merges_and_excludes_ai_assisted_ones() {
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
        if git(&["init", "-q"], "")
            .map(|s| !s.success())
            .unwrap_or(true)
        {
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
            &[
                "config",
                "remote.origin.url",
                "git@github.com:acme/myrepo.git",
            ],
            "",
        );
        write("a.txt", "1\n");
        let _ = git(&["add", "-A"], "");
        let _ = commit("init", "2025-01-01T00:00:00Z");
        let _ = git(&["branch", "-M", "base"], "");

        // A human PR: two commits 40 minutes apart, merged two days later.
        let _ = git(&["checkout", "-q", "-b", "feature"], "");
        write("a.txt", "1\n2\n3\n");
        let _ = git(&["add", "-A"], "");
        let _ = commit("wip one", "2025-01-02T00:00:00Z");
        write("b.txt", "x\ny\n");
        let _ = git(&["add", "-A"], "");
        let _ = commit("wip two", "2025-01-02T00:40:00Z");
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
            "2025-01-04T00:00:00Z",
        );

        // An AI PR: the trailer is on the BRANCH commit, not the merge, which is
        // exactly where a true merge hides it.
        let _ = git(&["checkout", "-q", "-b", "agentic"], "");
        write("c.txt", "ai\n");
        let _ = git(&["add", "-A"], "");
        let _ = commit(
            "wip agent\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
            "2025-02-01T00:00:00Z",
        );
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
                "Merge pull request #43 from acme/agentic",
                "agentic",
            ],
            "2025-02-02T00:00:00Z",
        );

        // A squash-merged AI PR: one parent, trailer on the merge commit itself.
        let _ = commit_empty(
            &dir,
            "feat: squashed (#44)\n\n🤖 Generated with [Claude Code](https://claude.com/claude-code)",
            "2026-01-01T00:00:00Z",
        );
        // A squash-merged HUMAN PR: an anchor with size but no span to claim.
        write("d.txt", "human\n");
        let _ = git(&["add", "-A"], "");
        let _ = commit("feat: squashed human (#45)", "2026-02-01T00:00:00Z");

        let mined = mine_repo_anchors(&path, &AnchorConfig::default()).unwrap();
        assert_eq!(mined.slug, "acme/myrepo");
        assert_eq!(mined.host.as_deref(), Some("github.com"));
        assert_eq!(mined.cutoff, None, "no date filter by default");
        assert_eq!(mined.head_sha.len(), 40);
        assert_eq!(
            mined
                .anchors
                .iter()
                .map(|a| a.pr_number)
                .collect::<Vec<_>>(),
            vec![45, 42],
            "newest first, and only the PRs no AI touched"
        );
        assert_eq!(mined.human_anchor_count, 2);
        assert_eq!(mined.ai_pr_count, 2, "#43 (branch) and #44 (squash)");
        assert!(mined.anchors.iter().all(|a| !a.ai_assisted));

        let squashed = &mined.anchors[0];
        assert_eq!(squashed.files_changed, 1);
        assert_eq!(squashed.lines_added, 1);
        // Its branch is gone: nothing is claimed rather than zero.
        assert_eq!(squashed.commit_count, None);
        assert_eq!(squashed.span_ms, None);
        assert_eq!(squashed.active_minutes, None);

        let merged = &mined.anchors[1];
        assert_eq!(merged.files_changed, 2);
        assert_eq!(merged.lines_added, 4);
        assert_eq!(merged.lines_deleted, 0);
        // A true merge: its branch is still here, so the span is real.
        assert_eq!(merged.commit_count, Some(2));
        assert_eq!(merged.span_ms, Some(2 * 86_400_000));
        // …and the two commits are one 40-minute sitting, plus the ramp — not
        // the two days the PR spent waiting to be merged.
        assert_eq!(merged.active_minutes, Some(70));
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
