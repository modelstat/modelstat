/**
 * On-device verified-outcome detection — the modelstat-native version of the
 * CPVO harness's "script git" loader (github.com/sneg55/cpvo). Given a PR the
 * session referenced and the local repo on disk, determine whether it merged
 * (and when) and whether it was later reverted — *without* a GitHub token or
 * webhook. The server's CPVO engine then classifies verified/failed from these
 * signals over the N-day survival window.
 *
 * Heuristic + git-only: it relies on GitHub's merge/squash commit-message
 * convention (`… (#123)` or `Merge pull request #123`) and `git revert`'s
 * `This reverts commit <sha>` body. The pure parsers below are exhaustively
 * unit-tested; the `git` invocation is a thin, best-effort wrapper.
 */
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const pexec = promisify(execFile);

/** What the local git history says about one PR's fate. */
export interface PrOutcome {
  /** A merge commit for the PR was found in history. */
  merged: boolean;
  /** ISO-8601 committer date of that merge commit, or null. */
  merged_at: string | null;
  /** A later commit reverts the merge (`This reverts commit <sha>`). */
  reverted: boolean;
}

/** One parsed commit. */
export interface GitCommit {
  sha: string;
  /** Committer date, ISO-8601 (`%cI`). */
  committedAt: string;
  subject: string;
  body: string;
}

// Field separator (US) between commit fields, record separator (RS) between
// commits — both bytes that never occur in commit text.
const FS = "\x1f";
const RS = "\x1e";
const GIT_LOG_FORMAT = `%H${FS}%cI${FS}%s${FS}%b${RS}`;

/** Parse `git log --format=<GIT_LOG_FORMAT>` output into commits. Pure. */
export function parseGitLog(stdout: string): GitCommit[] {
  const out: GitCommit[] = [];
  for (const record of stdout.split(RS)) {
    const rec = record.trim();
    if (!rec) continue;
    const [sha = "", committedAt = "", subject = "", ...rest] = rec.split(FS);
    out.push({ sha, committedAt, subject, body: rest.join(FS) });
  }
  return out;
}

/** The merge/squash commit for a PR number, or null. A GitHub merge subject is
 * `Merge pull request #123 from …`; a squash subject ends `… (#123)`. We match
 * `#<n>` with non-digit boundaries so `#123` doesn't also hit `#1234`. Pure. */
export function findMergeCommitForPr(commits: GitCommit[], prNumber: number): GitCommit | null {
  const re = new RegExp(`(^|\\D)#${prNumber}(\\D|$)`);
  return commits.find((c) => re.test(c.subject)) ?? null;
}

/** Whether any commit reverts `mergeSha` — `git revert` writes
 * `This reverts commit <full-or-short-sha>` into the body. Pure. */
export function isReverted(commits: GitCommit[], mergeSha: string): boolean {
  if (!mergeSha) return false;
  const short = mergeSha.slice(0, 7);
  return commits.some(
    (c) =>
      c.body.includes(`This reverts commit ${mergeSha}`) ||
      c.body.includes(`This reverts commit ${short}`),
  );
}

/** Classify a PR's outcome from already-parsed commits. Pure — the testable
 * core of {@link checkPullRequestOutcome}. */
export function outcomeFromCommits(commits: GitCommit[], prNumber: number): PrOutcome {
  const merge = findMergeCommitForPr(commits, prNumber);
  if (!merge) return { merged: false, merged_at: null, reverted: false };
  return {
    merged: true,
    merged_at: merge.committedAt || null,
    reverted: isReverted(commits, merge.sha),
  };
}

/**
 * Run `git log` in `cwd` and determine the PR's outcome. Best-effort: returns
 * null when `cwd` isn't a git repo or git fails (so the caller leaves the PR's
 * lifecycle fields unknown). Bounded to recent history + a short timeout.
 */
export async function checkPullRequestOutcome(
  cwd: string,
  prNumber: number,
): Promise<PrOutcome | null> {
  try {
    const { stdout } = await pexec("git", ["log", "-n", "1000", `--format=${GIT_LOG_FORMAT}`], {
      cwd,
      timeout: 4_000,
      maxBuffer: 16 * 1024 * 1024,
    });
    return outcomeFromCommits(parseGitLog(stdout), prNumber);
  } catch {
    return null;
  }
}
