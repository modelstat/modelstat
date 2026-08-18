//! On-device per-session file-change capture. The raw signal behind the token
//! re-spend heatmap: lines added/deleted per file across the commits in a
//! session's window (git only, no forge API).
//!
//! Privacy: git emits repo-relative paths + integer line counts only — no file
//! contents and no absolute/home paths (same public-shape class as a slug).

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;

use crate::git::run_git;

/// One file's aggregated line delta across the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Repo-relative path (git emits repo-relative; never absolute).
    pub path: String,
    pub lines_added: u64,
    pub lines_deleted: u64,
}

/// Parse aggregated per-file line counts from `git log --numstat` output. A
/// numstat line is `<added>\t<deleted>\t<path>`; `-` marks a binary file (0
/// lines). Non-numstat lines are ignored; the same path from several commits
/// sums. Pure. Preserves first-seen order (JS `Map`).
pub fn parse_numstat(stdout: &str) -> Vec<FileChange> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^([0-9]+|-)\t([0-9]+|-)\t(.+)$").unwrap());
    let mut order: Vec<String> = Vec::new();
    let mut by_path: std::collections::HashMap<String, FileChange> =
        std::collections::HashMap::new();
    for line in stdout.split('\n') {
        let Some(c) = re.captures(line) else { continue };
        let path = c[3].to_string();
        let added = if &c[1] == "-" {
            0
        } else {
            c[1].parse().unwrap_or(0)
        };
        let deleted = if &c[2] == "-" {
            0
        } else {
            c[2].parse().unwrap_or(0)
        };
        match by_path.get_mut(&path) {
            Some(e) => {
                e.lines_added += added;
                e.lines_deleted += deleted;
            }
            None => {
                order.push(path.clone());
                by_path.insert(
                    path.clone(),
                    FileChange {
                        path,
                        lines_added: added,
                        lines_deleted: deleted,
                    },
                );
            }
        }
    }
    order
        .into_iter()
        .filter_map(|k| by_path.remove(&k))
        .collect()
}

/// Aggregate the files changed by commits authored in [`since`, `until`] in the
/// repo at `cwd` (ISO-8601 instants). Best-effort: None when `cwd` isn't a git
/// repo or git fails. Bounded to recent history + a 4s timeout.
pub fn collect_files_changed(cwd: &str, since: &str, until: &str) -> Option<Vec<FileChange>> {
    let stdout = run_git(
        &[
            "-c",
            "core.quotePath=false",
            "log",
            &format!("--since={since}"),
            &format!("--until={until}"),
            "--numstat",
            "--no-renames",
            "--format=%H",
            "-n",
            "2000",
        ],
        cwd,
        Duration::from_millis(4_000),
    )?;
    Some(parse_numstat(&stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_sums_and_binaries_are_zero() {
        let out = "abc123\n\
                   3\t1\tsrc/a.ts\n\
                   -\t-\tassets/logo.png\n\
                   def456\n\
                   2\t0\tsrc/a.ts\n";
        let changes = parse_numstat(out);
        assert_eq!(
            changes,
            vec![
                FileChange {
                    path: "src/a.ts".into(),
                    lines_added: 5,
                    lines_deleted: 1
                },
                FileChange {
                    path: "assets/logo.png".into(),
                    lines_added: 0,
                    lines_deleted: 0
                },
            ]
        );
    }
}
