/**
 * Pure verified-outcome detection (CPVO). The `git` invocation in
 * checkPullRequestOutcome is a thin wrapper; everything load-bearing — finding
 * the merge commit for a PR number (with #N boundary), revert detection, and
 * the git-log parse — is pure and pinned here.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  type GitCommit,
  findMergeCommitForPr,
  isReverted,
  outcomeFromCommits,
  parseGitLog,
} from "./git-outcome.js";

const FS = "\x1f";
const RS = "\x1e";

/** Build a `git log --format=%H%x1f%cI%x1f%s%x1f%b%x1e` style blob. */
function gitLog(commits: Array<[sha: string, date: string, subject: string, body: string]>): string {
  return commits.map(([sha, date, subject, body]) => `${sha}${FS}${date}${FS}${subject}${FS}${body}${RS}`).join("");
}

test("parseGitLog splits records + fields", () => {
  const out = parseGitLog(
    gitLog([
      ["abc123", "2026-06-01T10:00:00Z", "Merge pull request #42 from x", "body one"],
      ["def456", "2026-06-02T10:00:00Z", "Squash (#7)", "body two\nmore"],
    ]),
  );
  assert.equal(out.length, 2);
  assert.deepEqual(out[0], {
    sha: "abc123",
    committedAt: "2026-06-01T10:00:00Z",
    subject: "Merge pull request #42 from x",
    body: "body one",
  });
  assert.equal(out[1]?.body, "body two\nmore");
});

test("findMergeCommitForPr matches merge + squash subjects", () => {
  const commits = parseGitLog(
    gitLog([
      ["m1", "2026-06-01T10:00:00Z", "Merge pull request #42 from fe/x", ""],
      ["s1", "2026-06-02T10:00:00Z", "Add retry logic (#7)", ""],
    ]),
  );
  assert.equal(findMergeCommitForPr(commits, 42)?.sha, "m1");
  assert.equal(findMergeCommitForPr(commits, 7)?.sha, "s1");
});

test("findMergeCommitForPr respects #N boundaries (no #123 in #1234)", () => {
  const commits = parseGitLog(gitLog([["c", "2026-06-01T10:00:00Z", "Title (#1234)", ""]]));
  assert.equal(findMergeCommitForPr(commits, 123), null);
  assert.equal(findMergeCommitForPr(commits, 1234)?.sha, "c");
});

test("findMergeCommitForPr returns null when absent", () => {
  const commits = parseGitLog(gitLog([["c", "2026-06-01T10:00:00Z", "unrelated work", ""]]));
  assert.equal(findMergeCommitForPr(commits, 99), null);
});

test("isReverted detects full + short sha in a revert body", () => {
  const full: GitCommit[] = [
    { sha: "x", committedAt: "", subject: "Revert", body: "This reverts commit abcdef1234567890." },
  ];
  assert.equal(isReverted(full, "abcdef1234567890"), true);
  const short: GitCommit[] = [
    { sha: "x", committedAt: "", subject: "Revert", body: "This reverts commit abcdef1." },
  ];
  assert.equal(isReverted(short, "abcdef1234567890"), true);
  assert.equal(isReverted(short, "0000000deadbeef"), false);
});

test("outcomeFromCommits: merged + clean = verified-eligible", () => {
  const commits = parseGitLog(gitLog([["m1", "2026-06-01T10:00:00Z", "Merge pull request #42 from x", ""]]));
  assert.deepEqual(outcomeFromCommits(commits, 42), {
    merged: true,
    merged_at: "2026-06-01T10:00:00Z",
    reverted: false,
  });
});

test("outcomeFromCommits: merged then reverted", () => {
  const commits = parseGitLog(
    gitLog([
      ["m1", "2026-06-01T10:00:00Z", "Merge pull request #42 from x", ""],
      ["r1", "2026-06-05T10:00:00Z", 'Revert "thing"', "This reverts commit m1."],
    ]),
  );
  assert.equal(outcomeFromCommits(commits, 42).reverted, true);
});

test("outcomeFromCommits: no merge = not merged", () => {
  const commits = parseGitLog(gitLog([["c", "2026-06-01T10:00:00Z", "wip", ""]]));
  assert.deepEqual(outcomeFromCommits(commits, 42), { merged: false, merged_at: null, reverted: false });
});
