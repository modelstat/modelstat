/**
 * Daemon singleton lock.
 *
 * Prevents a second `modelstat start` (or a tray-launched second
 * instance) from running in parallel with the first. A second daemon
 * would cause:
 *   - duplicate /v1/ingest POSTs (server is idempotent, but wasteful)
 *   - competing scan passes that clobber the local file cursor
 *   - scrambled Live Activity (heartbeats from two PIDs interleave)
 *
 * Mechanism: a lockfile at ~/.modelstat/daemon.lock containing
 *   { pid, startedAt, companionVersion, apiUrl }.
 *
 * On start:
 *   1. Read the existing lock if any.
 *   2. `kill(pid, 0)` probes whether the owner is alive. If yes, the
 *      caller is told to stop that one first (or the new invocation
 *      becomes a no-op).
 *   3. If the lock exists but the owner is gone, it's stale — we take
 *      it over.
 *   4. Write our own lock via atomic `open(O_CREAT|O_EXCL) → rename`.
 *   5. Register SIGINT / SIGTERM / beforeExit / uncaughtException
 *      handlers that remove the lockfile on exit.
 *
 * This is NOT a bulletproof distributed lock — two daemons started
 * within the same millisecond could both win the rename race. For the
 * one-user-one-machine case it's more than enough.
 */
import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeFileSync,
  writeSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export interface LockMeta {
  pid: number;
  startedAt: string;
  companionVersion: string;
  apiUrl: string;
}

const LOCK_DIR = join(homedir(), ".modelstat");
const LOCK_FILE = join(LOCK_DIR, "daemon.lock");

export function isProcessAlive(pid: number): boolean {
  if (!pid || pid <= 0) return false;
  try {
    // Signal 0 doesn't actually send anything — it just returns normally
    // if the PID exists and we have permission to signal it, or throws
    // ESRCH / EPERM otherwise.
    process.kill(pid, 0);
    return true;
  } catch (e) {
    const code = (e as NodeJS.ErrnoException).code;
    // EPERM means the process exists but is owned by a different user
    // — treat as alive so we don't steal their lock.
    if (code === "EPERM") return true;
    return false;
  }
}

/** Read + validate the lockfile. Exported (with an injectable path)
 * for the supervision health probe and its tests — see supervise.ts. */
export function readDaemonLock(lockFile: string = LOCK_FILE): LockMeta | null {
  try {
    const raw = readFileSync(lockFile, "utf8");
    const obj = JSON.parse(raw) as Partial<LockMeta>;
    if (typeof obj.pid !== "number") return null;
    return {
      pid: obj.pid,
      startedAt: obj.startedAt ?? "unknown",
      companionVersion: obj.companionVersion ?? "unknown",
      apiUrl: obj.apiUrl ?? "unknown",
    };
  } catch {
    return null;
  }
}

/**
 * Atomically write the lockfile. `open(O_CREAT|O_EXCL)` on a tmp
 * path + rename guarantees that only one process wins if two are
 * racing, provided they pick different tmp paths (we randomise on PID).
 */
function writeLockAtomic(meta: LockMeta): void {
  mkdirSync(LOCK_DIR, { recursive: true });
  const tmp = `${LOCK_FILE}.${meta.pid}.${Date.now()}.tmp`;
  const fd = openSync(tmp, "wx"); // fails if tmp somehow exists
  try {
    writeSync(fd, JSON.stringify(meta, null, 2));
  } finally {
    closeSync(fd);
  }
  renameSync(tmp, LOCK_FILE);
}

function removeLockIfOwned(ownerPid: number): void {
  const lock = readDaemonLock();
  if (!lock) return;
  if (lock.pid !== ownerPid) return; // someone else took over; leave alone
  try {
    unlinkSync(LOCK_FILE);
  } catch {
    /* already gone */
  }
}

export type AcquireResult =
  | { kind: "acquired" }
  | { kind: "already_running"; owner: LockMeta; ageSec: number };

export interface AcquireOpts {
  companionVersion: string;
  apiUrl: string;
  /** If true, kill any existing owner (if alive) and take the lock.
   * Used by `modelstat start --force` to recover from a stuck daemon.
   * Supervisors (the tray) should NOT pass this blindly: a healthy
   * live owner must be adopted, not killed — see supervise.ts for the
   * adopt/spawn/replace decision. Blind --force is how two tray
   * instances made their daemons SIGTERM each other in a loop on
   * 2026-06-12, ending with zero daemons running. */
  force?: boolean;
  /** Called if, shortly after we "won" the lock, the lockfile turns
   * out to be owned by someone else. writeLockAtomic is last-write-
   * wins, not mutual exclusion — two unforced daemons that read
   * "no lock" in the same instant both think they acquired it. The
   * recheck makes the rename loser stand down instead of running as
   * a silent duplicate (double scans, clobbered cursors, interleaved
   * heartbeats). Default: log + exit 0 (a supervisor will adopt the
   * winner). */
  onLockLost?: (winner: LockMeta) => void;
}

/** How long after acquiring to re-read the lock and confirm we still
 * own it. Long enough that a racing daemon's write has landed, short
 * enough that a duplicate daemon does no meaningful double-work. */
export const LOCK_RECHECK_MS = 5_000;

/**
 * Try to acquire the singleton lock. Returns:
 *   - `{ kind: "acquired" }` — caller can proceed to run the daemon.
 *   - `{ kind: "already_running" }` — another live daemon owns the lock;
 *     caller should print a message and exit 0.
 *
 * On "acquired", the lock is owned by this process and will be removed
 * automatically on SIGINT/SIGTERM/exit.
 */
export function acquireDaemonLock(opts: AcquireOpts): AcquireResult {
  const existing = readDaemonLock();
  if (existing && isProcessAlive(existing.pid)) {
    if (!opts.force) {
      const ageSec = ageInSeconds(existing.startedAt);
      return { kind: "already_running", owner: existing, ageSec };
    }
    // --force: try to stop the live owner first.
    try {
      process.kill(existing.pid, "SIGTERM");
    } catch {
      /* process already gone or no permission — we'll try the rename anyway */
    }
  }
  // Either no lock, stale lock, or we just killed the owner.
  const meta: LockMeta = {
    pid: process.pid,
    startedAt: new Date().toISOString(),
    companionVersion: opts.companionVersion,
    apiUrl: opts.apiUrl,
  };
  writeLockAtomic(meta);

  // Confirm ownership after the dust settles. Two unforced daemons can
  // both pass the no-live-owner check above and both rename their lock
  // into place; whoever renamed LAST owns the file. The loser finds a
  // different pid here and stands down via onLockLost.
  const recheck = setTimeout(() => {
    const current = readDaemonLock();
    if (checkLockOwnership(process.pid, current) !== "lost") return;
    const winner = current as LockMeta;
    if (opts.onLockLost) {
      opts.onLockLost(winner);
      return;
    }
    // biome-ignore lint/suspicious/noConsole: one-line, user-visible explanation of why we exit
    console.log(
      `[modelstat] another daemon (pid ${winner.pid}, started ${winner.startedAt}) won the lock race — this instance is standing down.`,
    );
    process.exit(0);
  }, LOCK_RECHECK_MS);
  recheck.unref();

  // Clean up on exit. Multiple signal paths all route to the same
  // unlink — it's idempotent via removeLockIfOwned.
  const cleanup = (): void => removeLockIfOwned(process.pid);
  process.once("beforeExit", cleanup);
  process.once("SIGINT", () => {
    cleanup();
    // Re-raise default behaviour: exit with 130 (the conventional
    // SIGINT code) so shell chains see we were interrupted.
    process.exit(130);
  });
  process.once("SIGTERM", () => {
    cleanup();
    process.exit(143);
  });
  process.once("uncaughtException", (err) => {
    cleanup();
    // biome-ignore lint/suspicious/noConsole: intentional crash log
    console.error("modelstat daemon crashed:", err);
    process.exit(1);
  });

  return { kind: "acquired" };
}

/** What the post-acquire recheck concluded about the lockfile.
 *   - "owned":       lock is ours (or vanished) — keep running.
 *   - "winner_dead": someone overwrote it but they're already gone —
 *                    keep running; our cleanup-on-exit still applies.
 *   - "lost":        a LIVE rival owns the lock — stand down.
 * Pure decision (pid probe injectable) so the duplicate-daemon
 * convergence rule is unit-testable without touching ~/.modelstat. */
export type OwnershipCheck = "owned" | "lost" | "winner_dead";
export function checkLockOwnership(
  myPid: number,
  current: LockMeta | null,
  pidAlive: (pid: number) => boolean = isProcessAlive,
): OwnershipCheck {
  if (!current || current.pid === myPid) return "owned";
  if (!pidAlive(current.pid)) return "winner_dead";
  return "lost";
}

function ageInSeconds(iso: string): number {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return -1;
  return Math.max(0, Math.floor((Date.now() - t) / 1000));
}

/** Human-friendly age like "3m 12s" for log lines. */
export function formatAge(seconds: number): string {
  if (seconds < 0) return "?";
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  if (m < 60) return `${m}m ${s}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

/** Expose the lock path so `modelstat stop` + diagnostics can locate it. */
export function daemonLockPath(): string {
  return LOCK_FILE;
}

// Keep the filesystem import used regardless of tsup treeshake behavior.
void writeFileSync;
void existsSync;
