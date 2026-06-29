/**
 * On-device per-session file-change capture — the raw signal behind the
 * "token re-spend heatmap" (which code a session's spend actually churned).
 * Given a repo on disk and the session's time window, aggregate the lines
 * added/deleted per file across the commits authored in that window — git
 * only, no forge API.
 *
 * Privacy: git emits repo-relative paths + integer line counts only — no file
 * contents and no absolute/home paths, the same public-shape safety class as a
 * repo slug.
 *
 * `--no-renames` so a rename surfaces as a clean delete + add of full paths
 * (never a `{old => new}` compound path), which a per-file rollup can key on.
 * The pure parser below is unit-tested; the `git` invocation is a thin,
 * best-effort wrapper that mirrors {@link checkPullRequestOutcome}.
 */
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const pexec = promisify(execFile);

/** One file's aggregated line delta across the window. */
export interface FileChange {
  /** Repo-relative path (git emits repo-relative; never an absolute path). */
  path: string;
  lines_added: number;
  lines_deleted: number;
}

/** Parse aggregated per-file line counts from `git log --numstat` output. A
 * numstat line is `<added>\t<deleted>\t<path>`; `-` marks a binary file (counted
 * as 0 lines). Non-numstat lines (the per-commit `--format` header, blanks) are
 * ignored, so the same `(added, deleted, path)` from several commits sums. Pure. */
export function parseNumstat(stdout: string): FileChange[] {
  const byPath = new Map<string, FileChange>();
  for (const line of stdout.split("\n")) {
    const m = /^(\d+|-)\t(\d+|-)\t(.+)$/.exec(line);
    if (!m) continue;
    const path = m[3] as string;
    const added = m[1] === "-" ? 0 : Number(m[1]);
    const deleted = m[2] === "-" ? 0 : Number(m[2]);
    const e = byPath.get(path);
    if (e) {
      e.lines_added += added;
      e.lines_deleted += deleted;
    } else {
      byPath.set(path, { path, lines_added: added, lines_deleted: deleted });
    }
  }
  return [...byPath.values()];
}

/**
 * Aggregate the files changed by commits authored in [`since`, `until`] in the
 * repo at `cwd`. `since`/`until` are ISO-8601 instants (the session's window).
 * Best-effort: returns null when `cwd` isn't a git repo or git fails. Bounded
 * to recent history + a short timeout, like {@link checkPullRequestOutcome}.
 */
export async function collectFilesChanged(
  cwd: string,
  since: string,
  until: string,
): Promise<FileChange[] | null> {
  try {
    const { stdout } = await pexec(
      "git",
      [
        "-c",
        "core.quotePath=false",
        "log",
        `--since=${since}`,
        `--until=${until}`,
        "--numstat",
        "--no-renames",
        "--format=%H",
        "-n",
        "2000",
      ],
      { cwd, timeout: 4_000, maxBuffer: 16 * 1024 * 1024 },
    );
    return parseNumstat(stdout);
  } catch {
    return null;
  }
}
