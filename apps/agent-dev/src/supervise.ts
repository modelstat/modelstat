/**
 * Daemon supervision policy — how an external supervisor (the macOS
 * tray, or anything else that keeps `modelstat start` alive) decides
 * what to do about the daemon singleton.
 *
 * Why this exists: the tray used to spawn `modelstat start --force`
 * unconditionally, and --force SIGTERMs whatever live daemon owns the
 * lock (lock.ts). When two tray instances briefly coexist — a
 * `launchctl kickstart -k` racing a reinstall, or KeepAlive respawn
 * overlap — each tray's daemon force-killed the other's in a loop,
 * and the music stopped with ZERO daemons running: a tray killed by
 * launchd never restarts its child, and the surviving tray's child
 * had just been SIGTERMed by the dying tray's last respawn. Observed
 * live 2026-06-12 ~17:26 — the daemon stayed down for over an hour
 * while its tray kept running.
 *
 * The policy here replaces blind --force with a three-way decision,
 * computed from two artifacts the daemon already maintains:
 *   - ~/.modelstat/daemon.lock        — { pid, startedAt, ... }
 *   - ~/.modelstat/last-status.json   — heartbeat mirror, `written_at`
 *                                       refreshed at least every ~10 s
 *
 *   adopt   — a live daemon owns the lock and is heartbeating (or is
 *             too young to judge). Leave it alone. THIS is what breaks
 *             the murder loop: a supervisor whose child was replaced
 *             backs off instead of counter-killing.
 *   spawn   — nobody owns the lock (none, or owner pid is dead).
 *             Plain `modelstat start`; lock.ts already treats a
 *             dead-owner lock as stale, no --force needed.
 *   replace — a live owner exists but hasn't heartbeat for
 *             STATUS_FRESH_MS and isn't a fresh boot: it's wedged.
 *             `modelstat start --force` takes it over.
 *
 * The tray consumes this via `modelstat _daemon-health` (cli.ts),
 * which prints the DaemonHealth JSON; the decision logic lives here,
 * in TypeScript, so it's unit-testable (`node --import tsx --test`)
 * — the Swift side stays a thin "run command, switch on decision".
 */

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { isProcessAlive, type LockMeta, readDaemonLock } from "./lock.js";

/** Owner is "healthy" while its heartbeat mirror is younger than this.
 * The daemon rewrites last-status.json on every heartbeat (10 s) and
 * on every status change, so 120 s = 12 missed heartbeats — wedged,
 * not busy. (Model load and long summarise passes keep the event loop
 * alive; heartbeats fire throughout.) */
export const STATUS_FRESH_MS = 120_000;

/** A lock younger than this is a daemon still booting (node startup,
 * imports, first status write). Adopt it even if the status mirror is
 * stale — the stale file belongs to a previous instance. This is the
 * window that lets a replacement daemon survive the dying supervisor's
 * final respawn check instead of being counter-killed. */
export const BOOT_GRACE_MS = 90_000;

export type SuperviseDecision = "adopt" | "spawn" | "replace";

export interface DaemonHealth {
  decision: SuperviseDecision;
  lock: LockMeta | null;
  ownerAlive: boolean;
  /** ms since the lock was written; null when unparseable. */
  lockAgeMs: number | null;
  /** ms since last-status.json was written; null when missing/unparseable. */
  statusAgeMs: number | null;
}

/** Pure decision over already-gathered facts — the unit-testable core. */
export function decideSupervision(input: {
  lock: LockMeta | null;
  ownerAlive: boolean;
  lockAgeMs: number | null;
  statusAgeMs: number | null;
  /** The probing CLI's own compiled-in companion version. When set and the
   * live owner's lock carries a DIFFERENT version, the owner is
   * replaced even though it's healthy: after an upgrade re-stages the
   * bundle, the old-version daemon would otherwise be adopted forever
   * — it survives launchd's group kill (observed 2026-06-12: a
   * kickstart left the old daemon running and the new tray adopted
   * it), heartbeats happily, and never picks up the new code. */
  myCompanionVersion?: string;
  statusFreshMs?: number;
  bootGraceMs?: number;
}): SuperviseDecision {
  const fresh = input.statusFreshMs ?? STATUS_FRESH_MS;
  const grace = input.bootGraceMs ?? BOOT_GRACE_MS;
  if (!input.lock || !input.ownerAlive) return "spawn";
  if (
    input.myCompanionVersion &&
    input.lock.companionVersion !== "unknown" &&
    input.lock.companionVersion !== input.myCompanionVersion
  ) {
    return "replace";
  }
  if (input.statusAgeMs !== null && input.statusAgeMs <= fresh) return "adopt";
  // Live owner, stale (or missing) heartbeat. A fresh boot hasn't had
  // the chance to write one — give it the grace window. An unparseable
  // lock age counts as NOT young: we can't vouch for it.
  if (input.lockAgeMs !== null && input.lockAgeMs <= grace) return "adopt";
  return "replace";
}

/** Gather the facts from disk and decide. All inputs injectable for
 * tests; production callers use the defaults. */
export function daemonHealth(
  opts: {
    lockPath?: string;
    statusPath?: string;
    now?: number;
    pidAlive?: (pid: number) => boolean;
    /** See decideSupervision.myCompanionVersion. cli.ts passes its
     * compiled-in AGENT_VERSION so an upgraded bundle replaces a
     * still-running old-version daemon. */
    myCompanionVersion?: string;
  } = {},
): DaemonHealth {
  const now = opts.now ?? Date.now();
  const pidAlive = opts.pidAlive ?? isProcessAlive;
  const statusPath = opts.statusPath ?? join(homedir(), ".modelstat", "last-status.json");

  const lock = opts.lockPath === undefined ? readDaemonLock() : readDaemonLock(opts.lockPath);
  const ownerAlive = lock !== null && pidAlive(lock.pid);
  const lockAgeMs = lock ? ageMs(lock.startedAt, now) : null;

  let statusAgeMs: number | null = null;
  try {
    const raw = readFileSync(statusPath, "utf8");
    const writtenAt = (JSON.parse(raw) as { written_at?: string }).written_at;
    statusAgeMs = writtenAt ? ageMs(writtenAt, now) : null;
  } catch {
    statusAgeMs = null;
  }

  return {
    decision: decideSupervision({
      lock,
      ownerAlive,
      lockAgeMs,
      statusAgeMs,
      myCompanionVersion: opts.myCompanionVersion,
    }),
    lock,
    ownerAlive,
    lockAgeMs,
    statusAgeMs,
  };
}

function ageMs(iso: string, now: number): number | null {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return null;
  return Math.max(0, now - t);
}
