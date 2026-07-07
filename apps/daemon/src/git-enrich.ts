/**
 * Authoritative git-remote enrichment — run over a batch of parsed events
 * BEFORE segmentation so the project identity keys on the real repository.
 *
 * The parsers set `git.remote_slug` from {@link guessRepoSlugFromPath}, a pure
 * path heuristic: it matches a container word (`www` | `src` | `code` | `repos`
 * | `projects`) anywhere in the cwd and takes the NEXT two path segments as
 * `owner/repo`. That misfires whenever the container word is a repo-INTERNAL
 * directory — a session run inside a repo's own `…/src/accounting/controllers`
 * tree reports the SUBDIRECTORY `accounting/controllers` as if it were a
 * repository, which then surfaces as a bogus row in "Spend by Repo" (the
 * `projects` hint → the `workstreams` taxonomy dimension).
 *
 * The repo is a hard known fact, not a guess: {@link resolveGitContext} reads
 * the actual `remote.origin.url` from the nearest `.git` on disk (collapsing
 * ephemeral `.claude` worktrees to their main repo, cwd-cached for the process,
 * so ~one `git` call per distinct repo). We overwrite each event's repo
 * identity (slug/host/url) with that authoritative value and leave the parser's
 * path guess as the last resort ONLY for a cwd that resolves no remote (a repo
 * with no origin, or one already gone from disk).
 *
 * The HISTORICAL branch the parser captured for the turn is preserved — env
 * inference and branch-ticket detection want the branch as it was at session
 * time, not today's checkout. Only the repo identity is corrected.
 */
import type { GitContext, RawEvent } from "@modelstat/core";
import { resolveGitContext } from "@modelstat/parsers";

/** Resolve a cwd to its on-disk git context. Injected in tests; defaults to the
 * parsers' real, cwd-cached resolver. */
export type GitResolver = (cwd: string | null) => Promise<GitContext | null>;

export async function resolveAuthoritativeGit(
  events: RawEvent[],
  resolveGit: GitResolver = resolveGitContext,
): Promise<RawEvent[]> {
  // One resolution per DISTINCT cwd (the resolver is cwd-cached anyway).
  const cwds = new Set<string>();
  for (const e of events) if (e.cwd) cwds.add(e.cwd);
  if (cwds.size === 0) return events;

  const resolved = new Map<string, GitContext>();
  for (const cwd of cwds) {
    let g: GitContext | null = null;
    try {
      g = await resolveGit(cwd);
    } catch {
      g = null; // best-effort — a failing git read keeps the parsed guess
    }
    // Only override when disk gives a real remote slug; otherwise the parser's
    // guess is still the best signal we have.
    if (g?.remote_slug) resolved.set(cwd, g);
  }
  if (resolved.size === 0) return events;

  return events.map((e) => {
    const g = e.cwd ? resolved.get(e.cwd) : undefined;
    if (!g) return e;
    return {
      ...e,
      git: {
        remote_url: g.remote_url,
        remote_host: g.remote_host,
        remote_slug: g.remote_slug,
        // Keep the branch the parser recorded for THIS turn; fall back to the
        // on-disk branch only when the event carried none.
        branch: e.git?.branch ?? g.branch,
      },
    };
  });
}
