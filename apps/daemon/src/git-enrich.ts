/**
 * Authoritative git-remote enrichment — run over a batch of parsed events
 * BEFORE segmentation so the project identity keys on the real repository.
 *
 * The parsers seed `git.remote_slug` from {@link guessRepoSlugFromPath}, a pure
 * path heuristic: it matches a container word (`www` | `src` | `code` | `repos`
 * | `projects`) anywhere in the cwd and takes the NEXT two path segments as
 * `owner/repo`. That misfires whenever the container word is a repo-INTERNAL
 * directory — a session run inside a repo's own `…/src/app/case-studies` tree
 * reports the SUBDIRECTORY `app/case-studies` as if it were a repository, which
 * surfaces as a bogus row in "Spend by Repo" (the `projects` hint → the
 * `workstreams` taxonomy dimension).
 *
 * The repo is a hard fact, not a guess. For each cwd we resolve, in order:
 *   1. the real `owner/repo` from `remote.origin.url` ({@link resolveGitContext},
 *      worktrees collapsed to the main repo, cwd-cached); else
 *   2. the repo-ROOT directory name ({@link resolveRepoRoot}) — a bare repo name
 *      that can NEVER be a subdirectory — for a repo with no remote (local-only,
 *      or origin unset).
 * Only when NO `.git` is reachable at all (a repo already deleted from disk by
 * parse time) do we leave the parser's path guess — which stays labeled
 * `path_shape`, so downstream never reads it as a verified identity.
 *
 * Both paths are ANCHORED on where `.git` actually is, so a subdirectory can
 * never be emitted — no hard-coded directory-name list. The HISTORICAL branch
 * the parser captured for the turn is preserved (env inference + branch-ticket
 * detection want the branch as it was); only the repo identity is corrected.
 */
import { basename } from "node:path";
import {
  type GitContext,
  type RawEvent,
  SLUG_SOURCE_GIT_REMOTE,
  SLUG_SOURCE_REPO_ROOT_DIR,
} from "@modelstat/core";
import { resolveGitContext, resolveRepoRoot } from "@modelstat/parsers";

/** Resolve a cwd to its on-disk git context. Injected in tests; defaults to the
 * parsers' real, cwd-cached resolver. */
export type GitResolver = (cwd: string | null) => Promise<GitContext | null>;
/** Resolve a cwd to its repo-root directory. Injected in tests; defaults to the
 * parsers' `.git`-walk resolver. */
export type RootResolver = (cwd: string | null) => string | null;

/** The corrected repo identity for a cwd (slug is always present here). */
interface RepoIdentity {
  remote_url: string | null;
  remote_host: string | null;
  remote_slug: string;
  branch: string | null;
  /** Which of the two corrections produced `remote_slug` — the configured
   * remote, or the repo-root directory name. Rides to the server so a bare
   * root name is never mistaken for an `owner/repo` off a real forge. */
  slug_source: string;
}

export async function resolveAuthoritativeGit(
  events: RawEvent[],
  resolveGit: GitResolver = resolveGitContext,
  resolveRoot: RootResolver = resolveRepoRoot,
): Promise<RawEvent[]> {
  // One resolution per DISTINCT cwd (both resolvers are cwd-cached anyway).
  const cwds = new Set<string>();
  for (const e of events) if (e.cwd) cwds.add(e.cwd);
  if (cwds.size === 0) return events;

  const resolved = new Map<string, RepoIdentity>();
  for (const cwd of cwds) {
    let g: GitContext | null = null;
    try {
      g = await resolveGit(cwd);
    } catch {
      g = null; // best-effort — a failing git read falls through to the root name
    }
    if (g?.remote_slug) {
      // Authoritative: a real `owner/repo` remote.
      resolved.set(cwd, {
        remote_url: g.remote_url,
        remote_host: g.remote_host,
        remote_slug: g.remote_slug,
        branch: g.branch,
        slug_source: SLUG_SOURCE_GIT_REMOTE,
      });
      continue;
    }
    // No remote → key on the repo-root directory NAME (bare, never a subpath)
    // instead of the parser's path guess.
    let root: string | null = null;
    try {
      root = resolveRoot(cwd);
    } catch {
      root = null;
    }
    const name = root ? basename(root) : "";
    if (name) {
      resolved.set(cwd, {
        remote_url: null,
        remote_host: null,
        remote_slug: name,
        branch: g?.branch ?? null,
        slug_source: SLUG_SOURCE_REPO_ROOT_DIR,
      });
    }
    // else: no `.git` reachable — leave the event's parsed value untouched.
  }
  if (resolved.size === 0) return events;

  return events.map((e) => {
    const id = e.cwd ? resolved.get(e.cwd) : undefined;
    if (!id) return e;
    return {
      ...e,
      git: {
        remote_url: id.remote_url,
        remote_host: id.remote_host,
        remote_slug: id.remote_slug,
        slug_source: id.slug_source,
        // Keep the branch the parser recorded for THIS turn; fall back to the
        // on-disk branch only when the event carried none.
        branch: e.git?.branch ?? id.branch,
      },
    };
  });
}
