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

export async function resolveGitContext(cwd: string | null): Promise<GitContext | null> {
  if (!cwd) return null;
  if (cache.has(cwd)) return cache.get(cwd) ?? null;
  const root = findRepoRoot(cwd);
  if (!root) {
    const empty: GitContext = {
      remote_url: null,
      remote_host: null,
      remote_slug: null,
      branch: null,
      commit_sha: null,
    };
    cache.set(cwd, empty);
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
  const sha = await ran(["rev-parse", "HEAD"]);

  const parsed = remoteUrl ? parseRemote(remoteUrl) : { host: null, slug: null };
  const ctx: GitContext = {
    remote_url: remoteUrl,
    remote_host: parsed.host,
    remote_slug: parsed.slug,
    branch,
    commit_sha: sha,
  };
  cache.set(cwd, ctx);
  return ctx;
}

/** Synchronous best-effort path→slug derivation, used when we cannot
 * invoke git (e.g. walking lots of old sessions). Heuristic only. */
export function guessRepoSlugFromPath(cwd: string | null): string | null {
  if (!cwd) return null;
  // common patterns: /Users/x/www/<org>/<repo> or /home/x/src/<org>/<repo>
  const m = /\/(?:www|src|code|repos|projects)\/([^/]+)\/([^/]+)/i.exec(cwd);
  if (m) return `${m[1]}/${m[2]}`;
  return null;
}
