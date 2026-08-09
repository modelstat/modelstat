//! On-device per-session commit capture — the direct-to-main half of the
//! spend→outcome join. A session that ships by pushing straight to `main` names
//! no PR and mentions no issue, so every reference channel misses it; the
//! commits it authored inside its window are the only handle the server has for
//! attributing that spend to an outcome.
//!
//! Privacy: this path reads TWO fields — `%H` (sha) and `%cI` (committer date).
//! Deliberately NOT [`crate::git_outcome::parse_git_log`], whose format string
//! also asks for `%s`/`%b`: commit subjects and bodies are free text an author
//! never expected to leave the machine, and the sha+timestamp pair is enough to
//! join spend to history. Do not widen the format string.

use std::time::Duration;

use crate::git::run_git;

/// One commit authored in a session's window — the two public facts about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// Full commit sha (`%H`).
    pub sha: String,
    /// Committer date, strict ISO-8601 (`%cI`) — the same clock `--since`/
    /// `--until` filter on, so the timestamp always sits inside the window.
    pub committed_at: String,
}

/// The `git log` format: sha, a space, ISO committer date. Neither field can
/// contain whitespace, so this needs none of the FS/RS escaping
/// [`crate::git_outcome`] uses to survive free-text subjects and bodies.
const COMMIT_FORMAT: &str = "--format=%H %cI";

/// Whether `s` looks like a commit sha — the same 7..=64 ASCII-hex shape
/// `references::valid_commit` enforces before anything reaches the wire, applied
/// here so a garbled line is dropped at the source rather than silently later.
fn is_sha(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse `git log --format=%H %cI` output into commits. Pure. Tolerant: blank
/// lines, a line missing its timestamp, and anything whose first field isn't a
/// sha are skipped rather than poisoning the batch.
pub fn parse_commit_shas(stdout: &str) -> Vec<CommitInfo> {
    stdout
        .split('\n')
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next()?;
            let committed_at = fields.next()?;
            is_sha(sha).then(|| CommitInfo {
                sha: sha.to_string(),
                committed_at: committed_at.to_string(),
            })
        })
        .collect()
}

/// The commits reachable from HEAD that were made in [`since`, `until`] (ISO-8601
/// instants) in the repo at `cwd`. Best-effort: None when `cwd` isn't a git repo
/// or git fails. Bounded to 200 commits + a 4s timeout — a session window that
/// somehow spans more than that is a merge of someone else's history, not work
/// to attribute.
pub fn collect_commits_authored(cwd: &str, since: &str, until: &str) -> Option<Vec<CommitInfo>> {
    let stdout = run_git(
        &[
            "log",
            // Explicit: only what the checked-out branch can reach — never every
            // ref in the repo (`--all` would claim a teammate's pushed branch).
            "HEAD",
            &format!("--since={since}"),
            &format!("--until={until}"),
            COMMIT_FORMAT,
            "--max-count=200",
        ],
        cwd,
        Duration::from_millis(4_000),
    )?;
    Some(parse_commit_shas(&stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, at: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.into(),
            committed_at: at.into(),
        }
    }

    #[test]
    fn parses_each_commit_line_in_order() {
        let out = "c0ffee1deadbeef1234567890abcdef123456789 2026-07-16T10:00:00+02:00\n\
                   abc1234 2026-07-16T09:00:00Z\n";
        assert_eq!(
            parse_commit_shas(out),
            vec![
                commit(
                    "c0ffee1deadbeef1234567890abcdef123456789",
                    "2026-07-16T10:00:00+02:00"
                ),
                commit("abc1234", "2026-07-16T09:00:00Z"),
            ]
        );
    }

    #[test]
    fn blank_and_truncated_lines_are_skipped() {
        // A blank line, a whitespace-only line, and a sha whose timestamp never
        // arrived (a truncated read) — none of them abort the rest.
        let out = "\n   \nabc1234\nabc1234def 2026-07-16T09:00:00Z\n";
        assert_eq!(
            parse_commit_shas(out),
            vec![commit("abc1234def", "2026-07-16T09:00:00Z")]
        );
    }

    #[test]
    fn non_sha_first_fields_are_dropped() {
        // Too short, non-hex, too long, and a git warning that reached stdout.
        let out = "abc123 2026-07-16T09:00:00Z\n\
                   not-hex-at-all 2026-07-16T09:00:00Z\n\
                   fatal: your branch is behind\n\
                   0123456789012345678901234567890123456789012345678901234567890123456789 2026-07-16T09:00:00Z\n\
                   deadbee 2026-07-16T09:00:00Z\n";
        assert_eq!(
            parse_commit_shas(out),
            vec![commit("deadbee", "2026-07-16T09:00:00Z")]
        );
    }

    #[test]
    fn empty_stdout_yields_no_commits() {
        assert!(parse_commit_shas("").is_empty());
        assert!(parse_commit_shas("\n").is_empty());
    }
}
