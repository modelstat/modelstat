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
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { platform } from "node:os";
import { homePath, modelstatHome } from "./paths.js";

const NPM = platform() === "win32" ? "npm.cmd" : "npm";
const PACKAGE = "modelstat";

function prefPath(): string {
  return homePath("auto-update.json");
}

/** Short-lived marker dropped the moment we spawn `npm i -g` so a daemon the
 * supervisor (launchd KeepAlive / systemd Restart) respawns ON THE SAME VERSION
 * while the install is mid-flight doesn't ALSO kick off its own `npm i -g`. The
 * postinstall stops + replaces the binary, so a respawn during that window is
 * expected; without this guard the respawn sees `upgrade_required` again and
 * stacks a second concurrent global install. */
function upgradeMarkerPath(): string {
  return homePath("upgrade-in-progress.json");
}

/** How long the upgrade marker is honoured. A normal stop→reinstall→restart
 * cycle finishes well inside this; after it lapses we assume the upgrade died
 * and allow a fresh attempt rather than wedging on a stale marker. */
const UPGRADE_MARKER_TTL_MS = 5 * 60_000;

/** Is `pid` a live process? `kill(pid, 0)` sends no signal — it only probes
 * existence. EPERM ⇒ alive but owned by another user (still counts as live);
 * ESRCH (or anything else) ⇒ treat as gone. */
function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

/** True when a recent upgrade marker exists — i.e. THIS process is very likely a
 * supervisor respawn during an in-flight `npm i -g`. The caller should skip
 * spawning another install and let the postinstall finish swapping the binary.
 *
 * Cleared (⇒ false, retry allowed) in two cases:
 *   · the marker is older than the TTL — a genuinely stuck upgrade; or
 *   · the `npm i -g` WORKER we spawned is already gone — the install it
 *     represents has finished or died, so there's nothing left to wait on.
 * We check the WORKER pid, NOT the daemon's: the daemon is deliberately
 * SIGTERM'd mid-upgrade by the postinstall, so its liveness says nothing — and
 * clearing on a dead daemon pid would let a respawn stack a second install
 * (exactly what this marker exists to prevent). */
export function upgradeInProgress(): boolean {
  try {
    const raw = readFileSync(upgradeMarkerPath(), "utf8");
    const o = JSON.parse(raw) as { at?: number; workerPid?: number };
    const at = Number(o?.at ?? 0);
    if (!(at > 0 && Date.now() - at < UPGRADE_MARKER_TTL_MS)) {
      clearUpgradeMarker(); // stale by the clock — don't let it wedge a retry
      return false;
    }
    const worker = Number(o?.workerPid ?? 0);
    if (worker > 0 && !pidAlive(worker)) {
      clearUpgradeMarker(); // the install worker is gone — finished or died
      return false;
    }
    return true;
  } catch {
    return false;
  }
}

/** Write the marker. `workerPid` is the `npm i -g` child once it's spawned
 * (null in the tiny window before, when the TTL is the only guard). */
function writeUpgradeMarker(target: string | null, workerPid: number | null = null): void {
  try {
    mkdirSync(modelstatHome(), { recursive: true, mode: 0o700 });
    const tmp = `${upgradeMarkerPath()}.${process.pid}.tmp`;
    writeFileSync(
      tmp,
      JSON.stringify({ pid: process.pid, workerPid, at: Date.now(), target: target ?? null }),
      { mode: 0o600 },
    );
    renameSync(tmp, upgradeMarkerPath());
  } catch {
    /* best-effort — the worst case without the marker is a duplicate install */
  }
}

/** Remove the upgrade marker. Called when an upgrade fails to start (so it
 * isn't blocked forever) and when a stale marker is detected. */
export function clearUpgradeMarker(): void {
  try {
    rmSync(upgradeMarkerPath(), { force: true });
  } catch {
    /* best-effort */
  }
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
export function runUpgrade(target: string | null = null): UpgradeResult {
  if (!canSelfUpdate()) {
    return { started: false, reason: "npm not found on PATH" };
  }
  // Drop the in-progress marker BEFORE spawning so a supervisor respawn that
  // races the install (postinstall stops + replaces us mid-flight) sees it and
  // doesn't stack a second `npm i -g`.
  writeUpgradeMarker(target);
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
    // Record the WORKER pid so a supervisor respawn can tell "install still
    // running" (worker alive) from "install finished/died" (worker gone) —
    // see upgradeInProgress().
    if (typeof child.pid === "number") writeUpgradeMarker(target, child.pid);
    return { started: true };
  } catch (e) {
    // The install never launched — clear the marker so the next verdict can
    // retry rather than being suppressed for the whole TTL.
    clearUpgradeMarker();
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
 *
 * `quiesce` (optional) runs RIGHT BEFORE the `npm i -g` spawn, only on the path
 * that actually upgrades. The daemon passes a callback that stops accepting new
 * scans, drains the in-flight scan, and frees the bundled summariser's Metal
 * device — so the to-be-killed process is already off the device when the
 * postinstall SIGTERMs it and the new daemon initialises Metal. Async, so this
 * function is async too.
 */
export async function maybeAutoUpdate(
  verdict: string,
  target: string | null,
  quiesce?: () => Promise<void>,
): Promise<string | null> {
  if (verdict !== "update_available" && verdict !== "upgrade_required") return null;
  // A supervisor respawn during an in-flight install must NOT stack a second
  // `npm i -g`. The marker is short-lived (TTL) so a genuinely stuck upgrade
  // still retries later.
  if (upgradeInProgress()) return null;
  const key = `${verdict}:${target ?? ""}`;
  if (handled.has(key)) return null;
  handled.add(key);

  const required = verdict === "upgrade_required";
  if (!autoUpdateEnabled()) {
    return `${required ? "upgrade required" : "update available"} (latest ${target ?? "?"}); auto-update is off — run \`modelstat upgrade\` or \`npm i -g modelstat@latest\``;
  }
  // Quiesce BEFORE spawning the install: free the Metal device so the
  // postinstall can SIGTERM us cleanly and the replacement daemon doesn't race
  // two processes onto the device. Best-effort — never block the upgrade on it.
  if (quiesce) {
    try {
      await quiesce();
    } catch {
      /* a failed drain shouldn't stop the upgrade — proceed */
    }
  }
  const r = runUpgrade(target);
  return r.started
    ? `auto-updating to ${target ?? "latest"} — \`npm i -g modelstat@latest\`; the service will restart on the new version`
    : `auto-update could not start (${r.reason}); upgrade manually: \`npm i -g modelstat@latest\``;
}
