//! On-device verified-outcome detection. Given a PR the session referenced and
//! the local repo on disk, determine whether it merged (and when) and whether it
//! was later reverted — git-only, no GitHub token.
//!
//! Heuristic: GitHub's merge/squash commit-message convention (`… (#123)` or
//! `Merge pull request #123`) and `git revert`'s `This reverts commit <sha>` body.

use std::time::Duration;

use regex::Regex;

use crate::git::run_git;

/// How `merged` was decided. One value today; a string rather than a bool so a
/// second method (a forge API, a `gh` tool result) does not need a new field.
pub const MERGE_METHOD_SUBJECT_REF: &str = "subject_ref_convention";

/// What the local git history says about one PR's fate — WITH the evidence.
///
/// `merged` is not an observation, it is a reading of a convention: a commit
/// subject that mentions `#<n>`. GitHub's default squash/merge templates write
/// one, so the reading is usually right, and it is wrong in both directions —
/// "Fix bug reported in #123" merges nothing, and a team with a custom squash
/// template merges without ever writing the number. Shipping the bare boolean
/// gave the server no way to notice either case, so the matched commit rides
/// along: `merge_sha` + `merge_subject` are already-public repo facts (the same
/// class as a slug), and `merge_method` names the reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrOutcome {
    pub merged: bool,
    pub merged_at: Option<String>,
    pub reverted: bool,
    /// The commit whose subject matched, when one did.
    pub merge_sha: Option<String>,
    /// That commit's subject line, verbatim — the text the claim rests on.
    pub merge_subject: Option<String>,
    /// Which reading produced `merged`; None when nothing matched.
    pub merge_method: Option<&'static str>,
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

/// Classify a PR's outcome from already-parsed commits. Pure.
pub fn outcome_from_commits(commits: &[GitCommit], pr_number: u64) -> PrOutcome {
    match find_merge_commit_for_pr(commits, pr_number) {
        None => PrOutcome {
            merged: false,
            merged_at: None,
            reverted: false,
            merge_sha: None,
            merge_subject: None,
            merge_method: None,
        },
        Some(merge) => PrOutcome {
            merged: true,
            merged_at: if merge.committed_at.is_empty() {
                None
            } else {
                Some(merge.committed_at.clone())
            },
            reverted: is_reverted(commits, &merge.sha),
            merge_sha: Some(merge.sha.clone()),
            // Verbatim: a subject the server can re-read is what makes the
            // convention checkable instead of trusted.
            merge_subject: Some(merge.subject.clone()),
            merge_method: Some(MERGE_METHOD_SUBJECT_REF),
        },
    }
}

/// Run `git log` in `cwd` and determine the PR's outcome. Best-effort: None when
/// `cwd` isn't a git repo or git fails. Bounded to recent history + a 4s timeout.
pub fn check_pull_request_outcome(cwd: &str, pr_number: u64) -> Option<PrOutcome> {
    let stdout = run_git(
        &[
            "log",
            "-n",
            "1000",
            &format!("--format={}", git_log_format()),
        ],
        cwd,
        Duration::from_millis(4_000),
    )?;
    Some(outcome_from_commits(&parse_git_log(&stdout), pr_number))
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
        assert!(outcome.merged);
        assert_eq!(outcome.merged_at.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert!(outcome.reverted);
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
        assert!(outcome.merged);
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
    fn no_merge_is_unmerged() {
        let commits = vec![commit("a", "t", "wip", "")];
        assert_eq!(
            outcome_from_commits(&commits, 99),
            PrOutcome {
                merged: false,
                merged_at: None,
                reverted: false,
                merge_sha: None,
                merge_subject: None,
                merge_method: None,
            }
        );
    }
}
