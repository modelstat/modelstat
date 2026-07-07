/**
 * Resolve a `cwd` into a git context (remote URL, host, slug, branch).
 * Safe to call on a non-repo — returns null fields.
 *
 * Uses Node's child_process sparingly — cached by cwd for the process
 * lifetime, since git lookups can be slow on macOS with iCloud paths.
 */
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { dirname, resolve } from "node:path";
import { existsSync } from "node:fs";
import type { GitContext } from "@modelstat/core";

const pexec = promisify(execFile);
const cache = new Map<string, GitContext>();

function findRepoRoot(startCwd: string): string | null {
  let cwd = resolve(startCwd);
  for (let i = 0; i < 10; i++) {
    if (existsSync(`${cwd}/.git`)) return cwd;
    const parent = dirname(cwd);
    if (parent === cwd) return null;
    cwd = parent;
  }
  return null;
}

function parseRemote(url: string): { host: string | null; slug: string | null } {
  // git@github.com:org/repo(.git)? OR https://github.com/org/repo(.git)?
  const ssh = /^(?:git@)?([^:]+):([^/]+)\/([^.]+?)(?:\.git)?$/.exec(url);
  if (ssh) return { host: ssh[1] ?? null, slug: `${ssh[2]}/${ssh[3]}` };
  try {
    const u = new URL(url);
    const m = /^\/([^/]+)\/([^/]+?)(?:\.git)?\/?$/.exec(u.pathname);
    if (!m) return { host: u.hostname, slug: null };
    return { host: u.hostname, slug: `${m[1]}/${m[2]}` };
  } catch {
    return { host: null, slug: null };
  }
}

/** The main-repo path for a (possibly ephemeral) worktree cwd: strips
 * `/.claude/worktrees/<id>` so a deleted worktree still resolves the real repo's
 * remote. A plain cwd is returned unchanged. */
export function mainRepoPath(cwd: string): string {
  const i = cwd.indexOf("/.claude/");
  return i === -1 ? cwd : cwd.slice(0, i);
}

/** The repo-root directory for a cwd — the nearest `.git` (worktrees collapsed
 * to the main repo via {@link mainRepoPath}), or null when none is reachable.
 * Deterministic + sync (an `existsSync` walk). Lets a caller derive a project
 * identity from where the repo actually IS, never a path-substring guess. */
export function resolveRepoRoot(cwd: string | null): string | null {
  if (!cwd) return null;
  return findRepoRoot(mainRepoPath(cwd));
}

export async function resolveGitContext(cwd: string | null): Promise<GitContext | null> {
  if (!cwd) return null;
  // Ephemeral worktrees live under `<repo>/.claude/worktrees/<id>` and are often
  // deleted by the time the daemon parses the session. Resolve from the MAIN repo
  // (`<repo>`, still on disk) so EVERY session of a repo yields the one canonical
  // remote slug — not a path-guess off the dead worktree, which split the project
  // dimension (`acme/acme` vs `acme`). Worktrees share the remote, so this is also
  // correct for live ones.
  const target = mainRepoPath(cwd);
  if (cache.has(target)) return cache.get(target) ?? null;
  const root = findRepoRoot(target);
  if (!root) {
    const empty: GitContext = {
      remote_url: null,
      remote_host: null,
      remote_slug: null,
      branch: null,
    };
    cache.set(target, empty);
    return empty;
  }

  const ran = async (args: string[]): Promise<string | null> => {
    try {
      const { stdout } = await pexec("git", args, { cwd: root, timeout: 2_000 });
      return stdout.trim() || null;
    } catch {
      return null;
    }
  };

  const remoteUrl = await ran(["config", "--get", "remote.origin.url"]);
  const branch = await ran(["rev-parse", "--abbrev-ref", "HEAD"]);

  const parsed = remoteUrl ? parseRemote(remoteUrl) : { host: null, slug: null };
  const ctx: GitContext = {
    remote_url: remoteUrl,
    remote_host: parsed.host,
    remote_slug: parsed.slug,
    branch,
  };
  cache.set(target, ctx);
  return ctx;
}

/** Synchronous best-effort path→slug derivation, used when we cannot
 * invoke git (e.g. walking lots of old sessions, or an ephemeral worktree that
 * was deleted before parse time). Heuristic only. */
export function guessRepoSlugFromPath(cwd: string | null): string | null {
  if (!cwd) return null;
  // common patterns: /Users/x/www/<org>/<repo> or /home/x/src/<org>/<repo>
  const m = /\/(?:www|src|code|repos|projects)\/([^/]+)\/([^/]+)/i.exec(cwd);
  if (!m) return null;
  const a = m[1];
  const b = m[2];
  if (!a || !b) return null;
  // `<b>` is worktree/tooling noise — e.g. `<repo>/.claude/worktrees/<id>` — not a
  // repo name: the project is just `<a>`. (Worktrees are ephemeral, so git often
  // can't resolve the real remote at parse time and we fall through to here.)
  if (b.startsWith(".") || b === "worktrees") return a;
  return `${a}/${b}`;
}
