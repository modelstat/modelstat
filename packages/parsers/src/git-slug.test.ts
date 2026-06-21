/**
 * Path→slug heuristic (`guessRepoSlugFromPath`), the fallback used when git
 * can't resolve a real remote — e.g. an ephemeral worktree under
 * `…/<repo>/.claude/worktrees/<id>` that's already deleted at parse time.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { guessRepoSlugFromPath, mainRepoPath } from "./git.js";

test("mainRepoPath: strips an ephemeral .claude worktree to the main repo", () => {
  // A worktree (deleted by parse time) resolves the MAIN repo's remote instead.
  assert.equal(
    mainRepoPath("/Users/dev/Projects/acme/.claude/worktrees/pensive-fermat-ea6051"),
    "/Users/dev/Projects/acme",
  );
  // A plain repo cwd is unchanged.
  assert.equal(mainRepoPath("/Users/dev/Projects/modelstat/core"), "/Users/dev/Projects/modelstat/core");
});

test("guessRepoSlugFromPath: owner/repo, but strips .claude/worktrees noise", () => {
  // A real owner/repo layout passes through.
  assert.equal(guessRepoSlugFromPath("/Users/x/Projects/modelstat/core/src"), "modelstat/core");
  // Ephemeral worktree noise collapses to the repo.
  assert.equal(guessRepoSlugFromPath("/Users/dev/Projects/acme/.claude/worktrees/x"), "acme");
  assert.equal(guessRepoSlugFromPath("/home/x/src/globex-infra/.claude"), "globex-infra");
  assert.equal(guessRepoSlugFromPath("/Users/x/Projects/acme/worktrees/y"), "acme");
  // No recognizable container → null.
  assert.equal(guessRepoSlugFromPath("/tmp/random/path"), null);
  assert.equal(guessRepoSlugFromPath(null), null);
});
