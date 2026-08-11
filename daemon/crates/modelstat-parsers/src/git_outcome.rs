//! On-device verified-outcome detection — a port of
//! `packages/parsers/src/git-outcome.ts`. Given a PR the session referenced and
//! the local repo on disk, determine whether it merged (and when) and whether it
//! was later reverted — git-only, no GitHub token.
//!
//! Heuristic: GitHub's merge/squash commit-message convention (`… (#123)` or
//! `Merge pull request #123`) and `git revert`'s `This reverts commit <sha>` body.

use std::time::Duration;

use regex::Regex;

use crate::git::{parse_numstat_totals, run_git};

/// How `merged` was decided. One value today; a string rather than a bool so a
/// second method (a forge API, a `gh` tool result) does not need a new field.
pub const MERGE_METHOD_SUBJECT_REF: &str = "subject_ref_convention";

/// What the local git history says about one PR's fate — WITH the evidence.
///
/// `merged` is three-state. `Some(true)` means a commit subject matched the PR
/// convention. `None` means this bounded local git read did not find one. Local
/// git cannot enumerate open PRs, so absence of a matching commit is never
/// reported as `Some(false)`; only a forge API can make that claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrOutcome {
    pub merged: Option<bool>,
    pub merged_at: Option<String>,
    pub reverted: Option<bool>,
    /// The commit whose subject matched, when one did.
    pub merge_sha: Option<String>,
    /// That commit's subject line, verbatim — the text the claim rests on.
    pub merge_subject: Option<String>,
    /// Which reading produced `merged`; None when nothing matched.
    pub merge_method: Option<&'static str>,
    /// What the merge commit CHANGED, when one was found and git could be read.
    /// Absent is "not measured" — see [`PrChange`].
    pub change: Option<PrChange>,
}

/// The change primitives of one PR, measured in the local repo — counts only.
///
/// These are the same numbers a forge reports for a PR, and the daemon can
/// read three of them off the merge commit `check_pull_request_outcome`
/// already found, with no token and no network. They exist ONLY when that
/// commit is in this repo: no repo on disk, no matching commit, or a failing
/// git read leaves the whole struct absent rather than zero. Zero is a
/// measurement ("this PR changed nothing"); absence is "nobody measured".
///
/// Privacy: counts. The numstat's path column is never captured
/// ([`parse_numstat_totals`]), so there is no field a path could be stored in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrChange {
    pub files_changed: u32,
    pub lines_added: u64,
    pub lines_deleted: u64,
    /// Commits the PR's branch carried — the range between the merge commit's
    /// two parents.
    ///
    /// `None` for a squash or rebase merge: it has ONE parent and the branch it
    /// flattened is not in this repo's history, so the commit count is not
    /// knowable here — while the numstat of what landed still is. Counting the
    /// squash commit itself as `1` would answer a different question ("commits
    /// on mainline") under the name of this one, and disagree with every forge.
    /// Same reading as `git_anchors::span_and_count`.
    pub commits_count: Option<u32>,
}

/// One parsed commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommit {
    pub sha: String,
    /// Committer date, ISO-8601 (`%cI`).
    pub committed_at: String,
    pub subject: String,
    pub body: String,
}

// Field separator (US) between commit fields, record separator (RS) between
// commits — bytes that never occur in commit text.
const FS: char = '\u{1f}';
const RS: char = '\u{1e}';

fn git_log_format() -> String {
    format!("%H{FS}%cI{FS}%s{FS}%b{RS}")
}

/// Parse `git log --format=<git_log_format>` output into commits. Pure.
pub fn parse_git_log(stdout: &str) -> Vec<GitCommit> {
    let mut out = Vec::new();
    for record in stdout.split(RS) {
        let rec = record.trim();
        if rec.is_empty() {
            continue;
        }
        let mut parts = rec.split(FS);
        let sha = parts.next().unwrap_or("").to_string();
        let committed_at = parts.next().unwrap_or("").to_string();
        let subject = parts.next().unwrap_or("").to_string();
        // body = the rest re-joined by FS (a body may itself contain no FS, but
        // this mirrors the TS `rest.join(FS)`).
        let body = parts.collect::<Vec<_>>().join(&FS.to_string());
        out.push(GitCommit {
            sha,
            committed_at,
            subject,
            body,
        });
    }
    out
}

/// The merge/squash commit for a PR number, or None. Matches `#<n>` with
/// non-digit boundaries so `#123` doesn't hit `#1234`. Pure.
pub fn find_merge_commit_for_pr(commits: &[GitCommit], pr_number: u64) -> Option<&GitCommit> {
    let re = Regex::new(&format!(r"(^|[^0-9])#{pr_number}([^0-9]|$)")).ok()?;
    commits.iter().find(|c| re.is_match(&c.subject))
}

/// Whether any commit reverts `merge_sha` (`git revert` body form). Pure.
pub fn is_reverted(commits: &[GitCommit], merge_sha: &str) -> bool {
    if merge_sha.is_empty() {
        return false;
    }
    let short = &merge_sha[..merge_sha.len().min(7)];
    commits.iter().any(|c| {
        c.body.contains(&format!("This reverts commit {merge_sha}"))
            || c.body.contains(&format!("This reverts commit {short}"))
    })
}

/// Classify a PR's outcome from already-parsed commits. Pure — so `change` is
/// always None here; [`check_pull_request_outcome`] is the half that can read
/// the merge commit's diff.
pub fn outcome_from_commits(commits: &[GitCommit], pr_number: u64) -> PrOutcome {
    match find_merge_commit_for_pr(commits, pr_number) {
        None => PrOutcome {
            merged: None,
            merged_at: None,
            reverted: None,
            merge_sha: None,
            merge_subject: None,
            merge_method: None,
            change: None,
        },
        Some(merge) => PrOutcome {
            merged: Some(true),
            merged_at: if merge.committed_at.is_empty() {
                None
            } else {
                Some(merge.committed_at.clone())
            },
            reverted: Some(is_reverted(commits, &merge.sha)),
            merge_sha: Some(merge.sha.clone()),
            // Verbatim: a subject the server can re-read is what makes the
            // convention checkable instead of trusted.
            merge_subject: Some(merge.subject.clone()),
            merge_method: Some(MERGE_METHOD_SUBJECT_REF),
            change: None,
        },
    }
}

/// The per-git-call ceiling for every read in this module.
const GIT_TIMEOUT: Duration = Duration::from_millis(4_000);

/// Run `git log` in `cwd` and determine the PR's outcome. Best-effort: `None`
/// when `cwd` isn't a git repo or every read failed.
///
/// **Two-stage, cheapest first.** A merge is usually reachable from the checked
/// out branch, and that walk is nearly free. Only when it is NOT found do we pay
/// for the wider one — because the caller's checkout is routinely on a feature
/// branch or behind the default branch, and a merge that landed on main is then
/// invisible to a HEAD-only walk (this is what reported every merged PR as not
/// merged).
///
/// The wide stage names the remote default branch rather than `--all`. Measured
/// on a real 164-ref repo on this (network-backed) disk: HEAD-only `0.03s`,
/// `--all` **`32.1s`** — three orders of magnitude, well past any sane ceiling,
/// so `--all` turned every PR in a big repo into a timeout. One extra ref is
/// where the merges actually are and costs a fraction of that.
///
/// When a merge commit is found, its change primitives are measured too
/// ([`measure_pr_change`]) — the sha is already in hand and the numbers are the
/// ones a forge would report, so a dashboard fed only by this daemon still has
/// them. A failure there costs the outcome nothing: `change` stays None.
pub fn check_pull_request_outcome(cwd: &str, pr_number: u64) -> Option<PrOutcome> {
    let fmt = format!("--format={}", git_log_format());
    let scan = |refs: &[String]| -> Option<PrOutcome> {
        let mut args: Vec<&str> = vec!["log", "--max-count=1000"];
        for r in refs {
            args.push(r);
        }
        args.push(&fmt);
        let stdout = run_git(&args, cwd, GIT_TIMEOUT)?;
        Some(outcome_from_commits(&parse_git_log(&stdout), pr_number))
    };

    // Stage 1: the checked out branch. Nearly free, and usually enough.
    let head = scan(&[]);
    let mut outcome = match head {
        Some(o) if o.merged == Some(true) => o,
        head => {
            // Stage 2: the handful of refs a merge actually lands on.
            let refs = integration_refs(cwd);
            match if refs.is_empty() { None } else { scan(&refs) } {
                // Only a positive finding beats stage 1; a wider walk that also
                // failed to find it adds no information.
                Some(w) if w.merged == Some(true) => w,
                _ => head?,
            }
        }
    };
    if let Some(sha) = outcome.merge_sha.as_deref() {
        outcome.change = measure_pr_change(cwd, sha);
    }
    Some(outcome)
}

/// The refs a merge plausibly lands on, filtered to the ones that exist: the
/// remote's default branch, and local/remote `main`/`master`.
///
/// Deliberately NOT `--all`. Naming a ref that does not exist makes git fail the
/// whole read, so each is probed with a cheap `rev-parse` first; and a repo with
/// many refs makes an all-refs walk pathologically slow — measured at 32.1s on a
/// real 164-ref repo on this disk, versus 0.03s for HEAD alone. A merge lives on
/// an integration branch, so those are the ones worth paying for.
///
/// Local names are included because a repo may have no remote configured at all
/// (a fresh `git init`, a mirror) while still carrying the merge on `main`.
fn integration_refs(cwd: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(name) = run_git(
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        cwd,
        GIT_TIMEOUT,
    ) {
        let name = name.trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    for r in ["origin/main", "origin/master", "main", "master"] {
        if out.iter().any(|o| o == r) {
            continue;
        }
        if run_git(&["rev-parse", "--verify", "--quiet", r], cwd, GIT_TIMEOUT)
            .is_some_and(|o| !o.trim().is_empty())
        {
            out.push(r.to_string());
        }
    }
    out
}

/// Measure what the commit `merge_sha` changed, in the repo at `cwd`. `None`
/// when the sha is not in this repo or git could not be read — never zeros.
///
/// `-m --first-parent` is load bearing: a merge commit's DEFAULT `git show`
/// diff is empty, which would report every true-merge PR as changing nothing.
/// It is a no-op on a squash merge's single parent, whose numstat is the whole
/// squashed change and is read exactly like any other commit's.
pub fn measure_pr_change(cwd: &str, merge_sha: &str) -> Option<PrChange> {
    let numstat = run_git(
        &[
            "-c",
            "core.quotePath=false",
            "show",
            "--numstat",
            "--format=",
            "-m",
            "--first-parent",
            merge_sha,
        ],
        cwd,
        GIT_TIMEOUT,
    )?;
    let (files_changed, lines_added, lines_deleted) = parse_numstat_totals(&numstat);
    Some(PrChange {
        files_changed,
        lines_added,
        lines_deleted,
        commits_count: branch_commit_count(cwd, merge_sha),
    })
}

/// How many commits the merged branch carried: `<sha>^1..<sha>^2`, the range
/// between the merge's two parents — the same set a forge lists on the PR.
///
/// A squash or rebase merge has no `^2`, so git exits non-zero and this is
/// None: the branch is gone from this history and the count cannot be read
/// from what remains. Indistinguishable from a git failure, and deliberately
/// so — both mean "not measured".
fn branch_commit_count(cwd: &str, merge_sha: &str) -> Option<u32> {
    let range = format!("{merge_sha}^1..{merge_sha}^2");
    let out = run_git(&["rev-list", "--count", &range], cwd, GIT_TIMEOUT)?;
    out.trim().parse().ok().filter(|n| *n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, at: &str, subject: &str, body: &str) -> GitCommit {
        GitCommit {
            sha: sha.into(),
            committed_at: at.into(),
            subject: subject.into(),
            body: body.into(),
        }
    }

    #[test]
    fn parse_and_classify_merge_and_revert() {
        let stdout = format!(
            "aaaa{FS}2026-01-02T00:00:00Z{FS}fix: thing (#123){FS}{RS}\
             bbbb{FS}2026-01-03T00:00:00Z{FS}revert: thing{FS}This reverts commit aaaa.{RS}"
        );
        let commits = parse_git_log(&stdout);
        assert_eq!(commits.len(), 2);
        let outcome = outcome_from_commits(&commits, 123);
        assert_eq!(outcome.merged, Some(true));
        assert_eq!(outcome.merged_at.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(outcome.reverted, Some(true));
        // The evidence for `merged` rides with it, verbatim.
        assert_eq!(outcome.merge_sha.as_deref(), Some("aaaa"));
        assert_eq!(outcome.merge_subject.as_deref(), Some("fix: thing (#123)"));
        assert_eq!(outcome.merge_method, Some(MERGE_METHOD_SUBJECT_REF));
    }

    #[test]
    fn a_prose_mention_is_shipped_with_the_subject_that_produced_it() {
        // The convention's known false positive: a subject that merely TALKS
        // about the PR. The daemon still reads it as merged (that is what the
        // convention says), but the server now sees exactly what it read.
        let commits = vec![commit("beef", "t", "fix: bug reported in #123", "")];
        let outcome = outcome_from_commits(&commits, 123);
        assert_eq!(outcome.merged, Some(true));
        assert_eq!(
            outcome.merge_subject.as_deref(),
            Some("fix: bug reported in #123")
        );
    }

    #[test]
    fn pr_number_boundary_is_respected() {
        let commits = vec![commit("a", "t", "chore: bump (#1234)", "")];
        assert!(find_merge_commit_for_pr(&commits, 123).is_none());
        assert!(find_merge_commit_for_pr(&commits, 1234).is_some());
    }

    #[test]
    fn no_merge_is_unknown() {
        let commits = vec![commit("a", "t", "wip", "")];
        assert_eq!(
            outcome_from_commits(&commits, 99),
            PrOutcome {
                merged: None,
                merged_at: None,
                reverted: None,
                merge_sha: None,
                merge_subject: None,
                merge_method: None,
                change: None,
            }
        );
    }

    /// End-to-end over a real repo: the git invocations are the half a pure
    /// test cannot check, and they are version-sensitive — a merge commit's
    /// DEFAULT `git show` diff is empty, so without `-m --first-parent` every
    /// true merge would ship zeros.
    #[test]
    #[cfg(unix)]
    fn change_primitives_come_off_the_merge_commit_or_not_at_all() {
        use std::process::{Command, Stdio};

        let dir = std::env::temp_dir().join(format!("modelstat-prchange-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_str().unwrap().to_string();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
                .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        };
        // Skip cleanly if git isn't available on this runner.
        if git(&["init", "-q"]).map(|s| !s.success()).unwrap_or(true) {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let commit = |message: &str| {
            git(&[
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
            ])
        };
        let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).unwrap();

        write("a.txt", "1\n");
        let _ = git(&["add", "-A"]);
        let _ = commit("init");
        let _ = git(&["branch", "-M", "base"]);

        // A true merge: two branch commits, one of them a binary file.
        let _ = git(&["checkout", "-q", "-b", "feature"]);
        write("a.txt", "1\n2\n3\n");
        let _ = git(&["add", "-A"]);
        let _ = commit("wip one");
        std::fs::write(dir.join("logo.png"), [0x89u8, 0x50, 0x4e, 0x47, 0x00, 0x01]).unwrap();
        let _ = git(&["add", "-A"]);
        let _ = commit("wip two");
        let _ = git(&["checkout", "-q", "base"]);
        let _ = git(&[
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
        ]);

        let merged = check_pull_request_outcome(&path, 42).expect("a repo git can read");
        assert_eq!(merged.merged, Some(true));
        let change = merged.change.expect("the merge commit is right here");
        // a.txt (+2) and the binary file: the binary row counts as a changed
        // file and adds no lines, rather than being dropped or read as zero.
        assert_eq!(change.files_changed, 2, "{change:?}");
        assert_eq!(change.lines_added, 2, "{change:?}");
        assert_eq!(change.lines_deleted, 0, "{change:?}");
        assert_eq!(change.commits_count, Some(2), "{change:?}");

        // A squash merge: the numstat IS readable, the flattened branch is not.
        write("d.txt", "one\ntwo\n");
        let _ = git(&["add", "-A"]);
        let _ = commit("feat: squashed (#43)");
        let squashed = check_pull_request_outcome(&path, 43)
            .and_then(|o| o.change)
            .expect("a squash merge's own diff is measurable");
        assert_eq!(
            (squashed.files_changed, squashed.lines_added),
            (1, 2),
            "{squashed:?}"
        );
        assert_eq!(
            squashed.commits_count, None,
            "the squashed branch is not in this history — nothing is claimed"
        );

        // A PR this repo never saw: the other fields still ship, the change
        // primitives are absent rather than zero.
        let absent = check_pull_request_outcome(&path, 9999).expect("still a readable repo");
        assert_eq!(absent.merged, None);
        assert_eq!(absent.change, None);
        // …and neither is a sha that is not in the repo at all.
        assert_eq!(
            measure_pr_change(&path, "0000000000000000000000000000000000000000"),
            None
        );
        // …nor is a repo that is not on disk at all.
        assert_eq!(
            measure_pr_change("/nonexistent-modelstat-prchange", "HEAD"),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end over a real repo: the merge commit may be reachable from a
    /// local ref even when the current checkout is on a branch that does not
    /// contain it. A HEAD-only bounded log incorrectly reports that as unknown.
    #[test]
    #[cfg(unix)]
    fn check_pull_request_outcome_reads_merge_from_non_head_ref() {
        use std::process::{Command, Stdio};

        let dir = std::env::temp_dir().join(format!("modelstat-prrefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_str().unwrap().to_string();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
                .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        // Skip cleanly if git isn't available on this runner.
        if !git(&["init", "-q"]) {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let commit = |message: &str| {
            git(&[
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
            ])
        };
        let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).unwrap();

        write("base.txt", "base\n");
        assert!(git(&["add", "-A"]));
        assert!(commit("init"));
        assert!(git(&["branch", "-M", "main"]));

        assert!(git(&["checkout", "-q", "-b", "feature"]));
        write("feature.txt", "landed\n");
        assert!(git(&["add", "-A"]));
        assert!(commit("feature work"));

        assert!(git(&["checkout", "-q", "main"]));
        assert!(git(&[
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
            "Merge pull request #77 from acme/feature",
            "feature",
        ]));

        assert!(git(&["checkout", "-q", "-b", "unrelated", "HEAD^1"]));
        write("unrelated.txt", "current branch\n");
        assert!(git(&["add", "-A"]));
        assert!(commit("unrelated work"));

        let outcome = check_pull_request_outcome(&path, 77).expect("a repo git can read");
        assert_eq!(outcome.merged, Some(true));
        assert_eq!(
            outcome.merge_subject.as_deref(),
            Some("Merge pull request #77 from acme/feature")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
