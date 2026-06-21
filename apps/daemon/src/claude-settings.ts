/**
 * Auto-configure Claude Code's user `settings.json` so `modelstat statusline`
 * renders at the bottom of every turn. A Claude Code PLUGIN cannot register the
 * main statusLine (only `subagentStatusLine`), so the installer is the only
 * mechanism — this runs as a step of `npx modelstat`.
 *
 * Idempotent + reversible:
 *   - Writing twice is a no-op once ours is in place.
 *   - If the user already has a DIFFERENT statusLine, we don't clobber it: we
 *     stash it under `_modelstatPrevStatusLine` and restore it on uninstall.
 *   - `removeStatusline()` removes ours and restores any stashed one.
 *
 * Best-effort: a parse/write failure is reported to the caller but never aborts
 * the install (the daemon's core duty is ingest, not the statusline).
 */
import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

/** The command we register. Resolved by the user's PATH to the installed
 * `modelstat` bin (the launchd/systemd install puts it there); kept bare so it
 * keeps working across upgrades that re-stage the bundle. */
export const STATUSLINE_COMMAND = "modelstat statusline";

/** Path to Claude Code's user settings. `CLAUDE_CONFIG_DIR` overrides the
 * default `~/.claude` (mirrors Claude Code's own resolution), so a custom
 * config dir or a test home is honoured. */
export function claudeSettingsPath(): string {
  const base = process.env.CLAUDE_CONFIG_DIR?.trim();
  return base && base.length > 0
    ? join(base, "settings.json")
    : join(homedir(), ".claude", "settings.json");
}

interface StatusLineSetting {
  type?: string;
  command?: string;
  padding?: number;
  [k: string]: unknown;
}

interface ClaudeSettings {
  statusLine?: StatusLineSetting;
  /** Where we stash a pre-existing foreign statusLine so uninstall can restore
   * it. Not a Claude Code field — namespaced so it can't collide. */
  _modelstatPrevStatusLine?: StatusLineSetting | null;
  [k: string]: unknown;
}

/** True when this statusLine setting is modelstat's (so re-install is a no-op
 * and uninstall only removes ours). Matches on our command substring so a
 * user-tweaked padding/refreshInterval still counts as ours. */
export function isModelstatStatusLine(s: StatusLineSetting | undefined | null): boolean {
  return typeof s?.command === "string" && s.command.includes("modelstat statusline");
}

/** Parse the settings file, tolerating absence (→ {}) and surfacing a real
 * parse error to the caller (so we never silently overwrite a file we couldn't
 * understand). */
function readSettings(path: string): ClaudeSettings {
  let raw: string;
  try {
    raw = readFileSync(path, "utf8");
  } catch {
    return {}; // no file yet
  }
  if (!raw.trim()) return {};
  const parsed = JSON.parse(raw) as unknown;
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new Error(`${path} is not a JSON object`);
  }
  return parsed as ClaudeSettings;
}

/** Atomically write settings (pretty-printed, trailing newline) via tmp+rename
 * so a crash mid-write can't truncate the user's config. Backs the previous
 * file up to `<path>.modelstat.bak` the first time we touch it. */
function writeSettings(path: string, settings: ClaudeSettings, hadFile: boolean): void {
  mkdirSync(dirname(path), { recursive: true });
  if (hadFile) {
    try {
      renameSync(path, `${path}.modelstat.bak`);
    } catch {
      /* best-effort backup — don't block the write */
    }
  }
  const tmp = `${path}.modelstat.tmp`;
  writeFileSync(tmp, `${JSON.stringify(settings, null, 2)}\n`);
  renameSync(tmp, path);
}

export type StatuslineInstallResult =
  | { kind: "installed"; preserved: boolean }
  | { kind: "already" }
  | { kind: "error"; message: string };

/**
 * Ensure `modelstat statusline` is the registered statusLine. Idempotent. If a
 * foreign statusLine is present it's stashed (and restored on uninstall) rather
 * than dropped.
 */
export function installStatusline(): StatuslineInstallResult {
  const path = claudeSettingsPath();
  let hadFile = true;
  let settings: ClaudeSettings;
  try {
    settings = readSettings(path);
    // Detect file presence so we only back up an existing file.
    try {
      readFileSync(path, "utf8");
    } catch {
      hadFile = false;
    }
  } catch (e) {
    return { kind: "error", message: (e as Error).message };
  }

  if (isModelstatStatusLine(settings.statusLine)) return { kind: "already" };

  let preserved = false;
  if (settings.statusLine?.command) {
    // Don't lose the user's existing statusLine — stash it for restore.
    settings._modelstatPrevStatusLine = settings.statusLine;
    preserved = true;
  }
  settings.statusLine = { type: "command", command: STATUSLINE_COMMAND, padding: 0 };

  try {
    writeSettings(path, settings, hadFile);
  } catch (e) {
    return { kind: "error", message: (e as Error).message };
  }
  return { kind: "installed", preserved };
}

export type StatuslineRemoveResult =
  | { kind: "removed"; restored: boolean }
  | { kind: "absent" }
  | { kind: "error"; message: string };

/**
 * Remove modelstat's statusLine. If we previously stashed a foreign one,
 * restore it; otherwise drop the statusLine entirely. Leaves a non-modelstat
 * statusLine untouched (we never installed over it).
 */
export function removeStatusline(): StatuslineRemoveResult {
  const path = claudeSettingsPath();
  let settings: ClaudeSettings;
  try {
    settings = readSettings(path);
  } catch (e) {
    return { kind: "error", message: (e as Error).message };
  }
  if (!isModelstatStatusLine(settings.statusLine)) {
    // Either nothing, or a statusLine that isn't ours — leave it be, but clean
    // up a dangling stash if one is somehow present.
    if (settings._modelstatPrevStatusLine !== undefined) {
      delete settings._modelstatPrevStatusLine;
      try {
        writeSettings(path, settings, true);
      } catch (e) {
        return { kind: "error", message: (e as Error).message };
      }
    }
    return { kind: "absent" };
  }

  let restored = false;
  const prev = settings._modelstatPrevStatusLine;
  if (prev?.command) {
    settings.statusLine = prev;
    restored = true;
  } else {
    delete settings.statusLine;
  }
  delete settings._modelstatPrevStatusLine;

  try {
    writeSettings(path, settings, true);
  } catch (e) {
    return { kind: "error", message: (e as Error).message };
  }
  return { kind: "removed", restored };
}
