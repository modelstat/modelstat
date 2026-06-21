/**
 * Daemon self-update.
 *
 * The server tells us our release standing in the heartbeat response (see
 * `handleRelease` in daemon.ts): `ok` / `update_available` / `upgrade_required`.
 * When we're behind and auto-update is on, we upgrade in place by spawning
 * `npm install -g modelstat@latest` detached — the package's postinstall then
 * stops, re-stages, and restarts the service (see scripts/postinstall.mjs), so
 * this very process may be killed mid-flight. That's expected.
 *
 * The auto-update preference lives in its OWN tiny file
 * (`<modelstatHome()>/auto-update.json`), NOT in state.json: the long-running
 * daemon caches state.json in memory and rewrites it on every cursor advance,
 * which would clobber a toggle written by a separate `modelstat autoupdate`
 * process (the tray). A dedicated file the daemon only ever reads (fresh, each
 * heartbeat) and the CLI only ever writes has no such race.
 */
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { platform } from "node:os";
import { homePath, modelstatHome } from "./paths.js";

const NPM = platform() === "win32" ? "npm.cmd" : "npm";
const PACKAGE = "modelstat";

function prefPath(): string {
  return homePath("auto-update.json");
}

/** The stored auto-update preference (default: on). Read fresh from disk so a
 * `modelstat autoupdate` toggle from another process is seen by the running
 * daemon without a restart. */
export function storedAutoUpdate(): boolean {
  try {
    const o = JSON.parse(readFileSync(prefPath(), "utf8")) as { autoUpdate?: boolean };
    return o.autoUpdate ?? true;
  } catch {
    return true; // no file yet ⇒ on by default
  }
}

/** Persist the auto-update preference atomically. */
export function setStoredAutoUpdate(enabled: boolean): void {
  mkdirSync(modelstatHome(), { recursive: true, mode: 0o700 });
  const tmp = `${prefPath()}.${process.pid}.tmp`;
  writeFileSync(tmp, JSON.stringify({ autoUpdate: enabled }), { mode: 0o600 });
  renameSync(tmp, prefPath());
}

/** Parse an env truthiness flag → true/false, or null when unset/unrecognised. */
function parseFlag(v: string | undefined): boolean | null {
  if (v == null) return null;
  const s = v.trim().toLowerCase();
  if (["0", "off", "false", "no", "disable", "disabled"].includes(s)) return false;
  if (["1", "on", "true", "yes", "enable", "enabled"].includes(s)) return true;
  return null;
}

/** Effective auto-update setting: the `MODELSTAT_AUTO_UPDATE` env override wins
 * (for managed fleets), else the stored preference. */
export function autoUpdateEnabled(): boolean {
  const env = parseFlag(process.env.MODELSTAT_AUTO_UPDATE);
  return env ?? storedAutoUpdate();
}

/** True when the setting is pinned by env (so the CLI/tray toggle is
 * informational only). */
export function autoUpdatePinnedByEnv(): boolean {
  return parseFlag(process.env.MODELSTAT_AUTO_UPDATE) !== null;
}

/** Can we self-update here? We need npm on PATH — the same assumption the
 * install + postinstall already make. */
export function canSelfUpdate(): boolean {
  try {
    return spawnSync(NPM, ["--version"], { stdio: "ignore", timeout: 10_000 }).status === 0;
  } catch {
    return false;
  }
}

export type UpgradeResult = { started: true } | { started: false; reason: string };

/**
 * Spawn `npm install -g modelstat@latest` detached and return immediately. The
 * postinstall hook does the stop → re-stage → restart, so the caller may be
 * killed mid-flight. Used by both `modelstat upgrade` (manual) and the
 * auto-updater. Best-effort: never throws.
 */
export function runUpgrade(): UpgradeResult {
  if (!canSelfUpdate()) {
    return { started: false, reason: "npm not found on PATH" };
  }
  try {
    // Scrub leaking npm_config_* (e.g. when invoked from inside an npm run) so
    // the global install resolves the real global prefix, not a nested one.
    const env = { ...process.env };
    delete env.npm_config_global;
    delete env.npm_config_prefix;
    const child = spawn(NPM, ["install", "-g", `${PACKAGE}@latest`], {
      detached: true,
      stdio: "ignore",
      env,
    });
    child.unref();
    return { started: true };
  } catch (e) {
    return { started: false, reason: (e as Error).message };
  }
}

// Per-launch guard: act/log at most once per (verdict, target). A successful
// upgrade replaces this process anyway; this stops the 10s heartbeat from
// re-spawning npm (or re-logging the "off" nudge) while one is in flight.
const handled = new Set<string>();

/**
 * React to a release verdict from the server. If auto-update is on, kick off the
 * upgrade; if off, return a one-time "upgrade manually" nudge. Returns a short
 * human-readable note to log the first time we see a given (verdict, target),
 * or null when there's nothing new to do. Never throws.
 */
export function maybeAutoUpdate(verdict: string, target: string | null): string | null {
  if (verdict !== "update_available" && verdict !== "upgrade_required") return null;
  const key = `${verdict}:${target ?? ""}`;
  if (handled.has(key)) return null;
  handled.add(key);

  const required = verdict === "upgrade_required";
  if (!autoUpdateEnabled()) {
    return `${required ? "upgrade required" : "update available"} (latest ${target ?? "?"}); auto-update is off — run \`modelstat upgrade\` or \`npm i -g modelstat@latest\``;
  }
  const r = runUpgrade();
  return r.started
    ? `auto-updating to ${target ?? "latest"} — \`npm i -g modelstat@latest\`; the service will restart on the new version`
    : `auto-update could not start (${r.reason}); upgrade manually: \`npm i -g modelstat@latest\``;
}
