/**
 * Path→slug heuristic (`guessRepoSlugFromPath`), the fallback used when git
 * can't resolve a real remote — e.g. an ephemeral worktree under
 * `…/<repo>/.claude/worktrees/<id>` that's already deleted at parse time.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { SLUG_SOURCE_PATH_SHAPE } from "@modelstat/core";
import { guessRepoSlugFromPath, mainRepoPath, pathGuessedGitContext } from "./git.js";

test("mainRepoPath: strips an ephemeral .claude worktree to the main repo", () => {
  // A worktree (deleted by parse time) resolves the MAIN repo's remote instead.
  assert.equal(
    mainRepoPath("/Users/dev/Projects/acme/.claude/worktrees/pensive-fermat-ea6051"),
    "/Users/dev/Projects/acme",
  );
  // A plain repo cwd is unchanged.
  assert.equal(mainRepoPath("/Users/dev/Projects/acme/web"), "/Users/dev/Projects/acme/web");
});

test("guessRepoSlugFromPath: owner/repo, but strips .claude/worktrees noise", () => {
  // A real owner/repo layout passes through.
  assert.equal(guessRepoSlugFromPath("/Users/x/Projects/acme/web/src"), "acme/web");
  // Ephemeral worktree noise collapses to the repo.
  assert.equal(guessRepoSlugFromPath("/Users/dev/Projects/acme/.claude/worktrees/x"), "acme");
  assert.equal(guessRepoSlugFromPath("/home/x/src/globex-infra/.claude"), "globex-infra");
  assert.equal(guessRepoSlugFromPath("/Users/x/Projects/acme/worktrees/y"), "acme");
  // No recognizable container → null.
  assert.equal(guessRepoSlugFromPath("/tmp/random/path"), null);
  assert.equal(guessRepoSlugFromPath(null), null);
});

test("pathGuessedGitContext: names no forge, labels the guess", () => {
  // Regression: the slug's `/` used to be read as "this is on GitHub" and
  // stamped into the same field `git config` fills. A directory layout is not
  // evidence of a host — self-hosted GitLab, Gitea, and Bitbucket all sit
  // behind the same `<org>/<repo>` shape.
  const ctx = pathGuessedGitContext("acme/web", null);
  assert.equal(ctx?.remote_slug, "acme/web");
  assert.equal(ctx?.remote_host, null);
  assert.equal(ctx?.remote_url, null);
  assert.equal(ctx?.slug_source, SLUG_SOURCE_PATH_SHAPE);
  // A branch with no slug is still worth shipping, and sources nothing.
  const branchOnly = pathGuessedGitContext(null, "main");
  assert.equal(branchOnly?.branch, "main");
  assert.equal(branchOnly?.slug_source, undefined);
  // Nothing observed at all → no context.
  assert.equal(pathGuessedGitContext(null, null), null);
});
