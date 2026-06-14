/**
 * Long-running daemon. Runs after `npx modelstat@latest` succeeds (or
 * directly via `modelstat start` if already paired).
 *
 * Responsibilities:
 *   1. Heartbeat every 10s so the dashboard knows this device is online.
 *   2. Discover installed tools once at startup (quick).
 *   3. Watch local AI-tool data dirs with chokidar; debounced full
 *      re-scan runs `scanAll()` on every change.
 *   4. Track per-file cursors in `conf` so a restart picks up exactly
 *      where we left off — no re-upload, no gaps.
 *   5. When the backend is unreachable, keep working: the cursor only
 *      advances on successful upload, so pending events accumulate
 *      and drain automatically when connectivity returns.
 *   6. Report phase + progress + queue size via heartbeat at every
 *      stage so the dashboard shows precise activity.
 */
import { existsSync, statSync } from "node:fs";
import { describeErrorWithCause } from "@modelstat/companion-core/logger";
import type { DiscoveryReport } from "@modelstat/core";
import { discover } from "@modelstat/parsers";
import { request } from "undici";
import { reportDiscovery } from "./api.js";
import { state } from "./config.js";
import { acquireDaemonLock, formatAge } from "./lock.js";
import { machineKey } from "./machine-key.js";
import { scanAll } from "./scan.js";
import { createCoalescingRunner } from "./single-flight.js";

// Substituted by tsup's `define` at build time (see tsup.config.ts).
// Replaces an older runtime parent-walk for package.json that broke
// once the bundle was copied to ~/.modelstat/bin/ (no sibling
// package.json), making the daemon report "agent-unknown" on every
// upgrade. cli.ts and scan.ts use the same macro.
const AGENT_VERSION: string =
  typeof __MODELSTAT_VERSION__ === "string" ? __MODELSTAT_VERSION__ : "agent-dev";
const HEARTBEAT_INTERVAL_MS = 10_000;
const SCAN_INTERVAL_MS = 5 * 60 * 1000; // backstop periodic scan
const DISCOVERY_INTERVAL_MS = 60_000; // re-enumerate installs + identities

type Phase =
  | "starting"
  | "discovering"
  | "idle"
  | "scanning"
  | "processing"
  | "uploading"
  | "watching"
  | "offline"
  | "error";

interface Heartbeat {
  status: Phase;
  message?: string | null;
  progress_done?: number;
  progress_total?: number;
  queue_size?: number;
  stats?: Record<string, number | string>;
  last_event_at?: string | null;
  companion_version?: string;
  /** Stable hardware machine key (see machine-key.ts). Sent so the
   * server can backfill `devices.machine_id` onto an already-enrolled
   * row that registered before machine_id existed — which is what lets
   * machine-key dedupe protect legacy devices without a re-register. */
  machine_id?: string;
}

/** Shared mutable state the heartbeat reporter reads. Each subsystem
 * (scanner, watcher, uploader) updates these; the reporter snapshots
 * them and sends via the heartbeat endpoint. */
const status = {
  phase: "starting" as Phase,
  message: null as string | null,
  progressDone: 0,
  progressTotal: 0,
  queueSize: 0,
  stats: {} as Record<string, number | string>,
  lastEventAt: null as string | null,
};

export function setPhase(phase: Phase, message?: string | null): void {
  status.phase = phase;
  status.message = message ?? null;
  scheduleLocalFlush();
}
/** Update only the human-readable status line (phase unchanged). */
export function setMessage(message: string | null): void {
  status.message = message;
  scheduleLocalFlush();
}
export function setProgress(done: number, total: number): void {
  status.progressDone = done;
  status.progressTotal = total;
  scheduleLocalFlush();
}
export function setQueue(n: number): void {
  status.queueSize = n;
  scheduleLocalFlush();
}
export function bumpStat(key: string, delta: number): void {
  const cur = Number(status.stats[key] ?? 0);
  status.stats[key] = cur + delta;
  scheduleLocalFlush();
}
/** Set a stat to an absolute value. Used for gauges like
 * `segments_sending` that go up then back to zero, where a running
 * delta would drift. */
export function setStat(key: string, value: number): void {
  status.stats[key] = value;
  scheduleLocalFlush();
}
export function noteEventAt(iso: string): void {
  status.lastEventAt = iso;
  scheduleLocalFlush();
}

/** Snapshot the shared mutable `status` into the wire/heartbeat shape.
 * Used by both the network heartbeat and the local-file mirror so the
 * two never drift. */
function snapshotBody(): Heartbeat & { device_id: string | null } {
  return {
    device_id: state.deviceId ?? null,
    status: status.phase,
    message: status.message,
    progress_done: status.progressDone,
    progress_total: status.progressTotal,
    queue_size: status.queueSize,
    stats: status.stats,
    last_event_at: status.lastEventAt,
    companion_version: AGENT_VERSION,
    machine_id: machineKey(),
  };
}

// Write last-status.json on every status change (coalesced), decoupled
// from the 10s network heartbeat. The tray reads this file directly, so
// without this the menu only ever moved once per heartbeat — and during
// a long summarise pass (no heartbeat-relevant change) it looked frozen
// for minutes even though work was happening.
const LOCAL_FLUSH_THROTTLE_MS = 400;
let localFlushTimer: NodeJS.Timeout | null = null;
let localFlushPending = false;
function scheduleLocalFlush(): void {
  if (localFlushTimer) {
    // A write is on cooldown; remember to flush again when it lifts so
    // the final state of a burst always lands.
    localFlushPending = true;
    return;
  }
  writeLocalStatus(snapshotBody()).catch(() => undefined); // leading edge
  localFlushTimer = setTimeout(() => {
    localFlushTimer = null;
    if (localFlushPending) {
      localFlushPending = false;
      scheduleLocalFlush(); // trailing edge — capture latest state
    }
  }, LOCAL_FLUSH_THROTTLE_MS);
  localFlushTimer.unref();
}

async function sendHeartbeat(): Promise<void> {
  const bearer = state.bearer;
  const deviceId = state.deviceId;
  if (!bearer || !deviceId) return; // pre-enrollment
  const body = { ...snapshotBody(), device_id: deviceId };
  try {
    const res = await request(`${state.apiUrl}/v1/agent/heartbeat`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${bearer}` },
      body: JSON.stringify(body),
    });
    if (res.statusCode >= 300) {
      // eat the body so we don't leak a handle
      await res.body.text();
    }
  } catch {
    // Network blip — dashboard will see stale heartbeat and mark us
    // offline. The *local* phase will switch via the scanner's catch
    // block on the next upload attempt.
  }
  // Mirror the heartbeat to ~/.modelstat/last-status.json so the
  // tray (and any other local consumer) can read fresh numbers
  // without an authenticated round-trip to the server. Critical for
  // CLAIMED devices: the public /v1/device/:claim_code endpoint 404s
  // for non-owner viewers, so the tray's `modelstat stats --json`
  // would otherwise see only `{paired, claimed, dashboard}` with no
  // segment / identity / installation counts.
  writeLocalStatus(body).catch(() => undefined);
}

/** Cap on ~/.modelstat/logs/{out,err}.log before boot-time rotation.
 * The 2026-06-11 incident left a 992 MB err.log (one identical warn
 * line repeated ~5M times during a full reprocess); nothing ever
 * trimmed it because launchd doesn't rotate StandardErrorPath. */
const LOG_MAX_BYTES = 64 * 1024 * 1024;
/** How much of the oversized log's tail survives into `<name>.old.log`
 * — enough to keep the most recent crash stacks for forensics. */
const LOG_TAIL_KEEP_BYTES = 4 * 1024 * 1024;

/**
 * Boot-time guard against runaway log files. Whoever supervises us —
 * launchd writing StandardErrorPath (O_APPEND) or the tray holding a
 * FileHandle — keeps the log fd OPEN across daemon restarts, so the
 * log must be truncated IN PLACE: renaming would detach the path from
 * the live fd and the supervisor would keep growing the renamed inode
 * forever. We copy the tail to `<name>.old.log` first so the last
 * crash isn't lost. Best-effort: any fs error leaves the log as-is.
 */
async function rotateRunawayLogs(): Promise<void> {
  const { homedir } = await import("node:os");
  const { join } = await import("node:path");
  const { open, stat, truncate, writeFile } = await import("node:fs/promises");
  const dir = join(homedir(), ".modelstat", "logs");
  for (const name of ["out.log", "err.log"]) {
    const p = join(dir, name);
    try {
      const st = await stat(p);
      if (st.size <= LOG_MAX_BYTES) continue;
      const keep = Math.min(LOG_TAIL_KEEP_BYTES, st.size);
      const fh = await open(p, "r");
      try {
        const buf = Buffer.alloc(keep);
        await fh.read(buf, 0, keep, st.size - keep);
        await writeFile(p.replace(/\.log$/, ".old.log"), buf);
      } finally {
        await fh.close();
      }
      await truncate(p, 0);
      // biome-ignore lint/suspicious/noConsole: one-line, post-rotation so it lands in the fresh log
      console.log(
        `[modelstat] rotated ${name}: was ${(st.size / 1024 / 1024).toFixed(0)} MB (> ${LOG_MAX_BYTES / 1024 / 1024} MB cap), tail kept in ${name.replace(/\.log$/, ".old.log")}`,
      );
    } catch {
      /* best-effort — never block daemon boot on log hygiene */
    }
  }
}

let lastStatusPath: string | null = null;
async function writeLocalStatus(snapshot: object): Promise<void> {
  const { homedir } = await import("node:os");
  const { join } = await import("node:path");
  const { writeFile, mkdir, rename } = await import("node:fs/promises");
  if (!lastStatusPath) {
    const dir = join(homedir(), ".modelstat");
    try {
      await mkdir(dir, { recursive: true });
    } catch {
      /* permission / fs error — silent; tray will fall back */
    }
    lastStatusPath = join(dir, "last-status.json");
  }
  const tmp = `${lastStatusPath}.tmp`;
  try {
    await writeFile(tmp, JSON.stringify({ ...snapshot, written_at: new Date().toISOString() }));
    await rename(tmp, lastStatusPath);
  } catch {
    /* fs blip — next tick will retry */
  }
}

/** Discover installs + report. Safe to call repeatedly. */
async function runDiscovery(): Promise<void> {
  const deviceId = state.deviceId;
  if (!deviceId) return;
  setPhase("discovering", "Enumerating local AI tools");
  try {
    const d = await discover();
    const report: DiscoveryReport = {
      device_id: deviceId,
      installations: d.installations,
      identities: d.identities,
      scanned_at: new Date().toISOString(),
    };
    await reportDiscovery(report);
    status.stats["installations_detected"] = d.installations.length;
    status.stats["identities_detected"] = d.identities.length;
  } catch (e) {
    setPhase("error", `discovery failed: ${describeErrorWithCause(e)}`);
  }
}

async function runScanCycle(reason: string): Promise<void> {
  setPhase("scanning", `Scanning local JSONL (${reason})`);
  setProgress(0, 0);
  try {
    const r = await scanAll({
      onFile(path, index, total) {
        setProgress(index + 1, total);
        setMessage(`Scanning ${index + 1}/${total}: ${basename(path)}`);
      },
      onProgress(p) {
        // The summarise pass — the slow, previously-opaque phase. Keep
        // the status line moving on every segment (and every session) so
        // the tray shows continuous activity instead of a frozen number.
        const sess = p.sessionTotal > 1 ? ` · session ${p.session}/${p.sessionTotal}` : "";
        if (p.segment === 0) {
          setPhase("processing", `Analyzing${sess}`);
        } else {
          setPhase("processing", `Summarising segment ${p.segment}/${p.segmentTotal}${sess}`);
        }
      },
      onUpload({ segments }) {
        // In-flight gauge: how many segments are being sent right now.
        setPhase("uploading", `Uploading ${segments} segments`);
        setStat("segments_sending", segments);
      },
      onUploaded({ events, segments }) {
        bumpStat("events_uploaded", events);
        bumpStat("batches_uploaded", 1);
        // Persist the lifetime total and mirror it into the heartbeat
        // so the tray's "total sent" survives restarts.
        setStat("segments_sent", state.bumpSegmentsSent(segments));
        setStat("segments_sending", 0);
        status.lastEventAt = new Date().toISOString();
      },
    });
    bumpStat("files_scanned", r.filesScanned);
    bumpStat("files_unchanged", r.filesUnchanged);
    setPhase("watching", "Waiting for new events");
    setProgress(0, 0);
  } catch (e) {
    // Unwrap undici's "fetch failed" wrapper so the dashboard shows
    // the actual cause (ECONNREFUSED, ENOTFOUND, certificate error,
    // etc) — the original message alone was useless for debugging.
    // A batch was in-flight when this threw, so clear the gauge.
    setStat("segments_sending", 0);
    setPhase("offline", `Upload failed: ${describeErrorWithCause(e)}`);
  }
}

/**
 * Single entry point for every scan trigger (startup, file-watcher,
 * 5-min backstop). Coalescing single-flight: never runs two scans at
 * once, and collapses triggers that arrive mid-scan into exactly one
 * follow-up. This is what bounds the daemon's memory — see
 * single-flight.ts for why an unguarded `runScanCycle` OOMs the V8 heap
 * during long backfills.
 */
const scanRunner = createCoalescingRunner<string>(runScanCycle);
function requestScan(reason: string): Promise<void> {
  return scanRunner.trigger(reason);
}

function basename(p: string): string {
  return p.split("/").pop() ?? p;
}

/** The main loop. Never returns unless blocked by an already-running
 * daemon (in which case returns 0 with a friendly "already running"
 * message and no side effects). */
export async function runDaemon(opts: { force?: boolean } = {}): Promise<void> {
  if (!state.bearer || !state.deviceId) {
    throw new Error("not enrolled — run `npx modelstat@latest` first");
  }

  // Singleton lock: bail out early if another daemon is already running
  // under this home directory. Two daemons clobber each other's file
  // cursors and send duplicate heartbeats that scramble Live Activity.
  const lock = acquireDaemonLock({
    companionVersion: AGENT_VERSION,
    apiUrl: state.apiUrl,
    force: opts.force === true,
    // If a racing daemon out-renamed us for the lock (see lock.ts
    // recheck), stand down with exit 0: our supervisor's next health
    // check sees the winner and adopts it instead of respawning us.
    onLockLost: (winner) => {
      setPhase("offline", `another daemon (pid ${winner.pid}) owns the lock — standing down`);
      // biome-ignore lint/suspicious/noConsole: one-line, user-visible exit reason
      console.log(
        `[modelstat] another daemon (pid ${winner.pid}, started ${winner.startedAt}) won the lock race — this instance is standing down.`,
      );
      process.exit(0);
    },
  });
  if (lock.kind === "already_running") {
    // biome-ignore lint/suspicious/noConsole: user-visible CLI output
    console.log(
      `modelstat daemon is already running — PID ${lock.owner.pid}, started ${formatAge(
        lock.ageSec,
      )} ago, agent ${lock.owner.companionVersion}.`,
    );
    // biome-ignore lint/suspicious/noConsole: user-visible CLI output
    console.log("  → to stop it:          kill " + lock.owner.pid);
    // biome-ignore lint/suspicious/noConsole: user-visible CLI output
    console.log("  → to force-replace it: modelstat start --force");
    return;
  }

  setPhase("starting", "Booting");
  // Trim runaway logs before anything else writes to them — a daemon
  // that died spamming one warn line must not boot back up on top of a
  // gigabyte-scale err.log.
  await rotateRunawayLogs();
  // Seed the lifetime "segments sent" tally from disk so the tray
  // shows the running total from the first heartbeat, not 0 until the
  // first upload of this run.
  setStat("segments_sent", state.segmentsSent);

  // Start heartbeat ticker immediately
  const hb = setInterval(() => void sendHeartbeat(), HEARTBEAT_INTERVAL_MS);
  hb.unref();
  void sendHeartbeat(); // prime

  // Verify the summariser end-to-end before producing any segments.
  // If this throws (the bundled node-llama-cpp runtime didn't load, or
  // the model file is bad), we'd rather the daemon refuse to start than
  // churn out useless metadata template abstracts ("100 turns on
  // claude_code") for hours. The service supervisor (launchd / systemd)
  // will retry per its throttle, and the user sees a real error in
  // ~/.modelstat/logs/.
  try {
    setPhase("starting", "Preflight: summariser");
    const { preflightSummariser } = await import("./pipeline.js");
    const sample = await preflightSummariser();
    // biome-ignore lint/suspicious/noConsole: startup status
    console.log(`[modelstat] summariser preflight ok: "${sample}"`);
  } catch (err) {
    setPhase("error", `summariser preflight failed: ${(err as Error).message}`);
    throw err;
  }

  // Reconcile the local processing-pipeline version. If this build
  // produces materially different segments than the one that wrote
  // the on-disk cursors (e.g. summariser model swap, prompt change),
  // wipe cursors so the next scan re-reads every JSONL from byte 0
  // and re-summarises every session. A re-scan REPLACES stale segments
  // by segment_id in place — no purge required for normal upgrades.
  const { reconcileProcessingVersion } = await import("./processing-version.js");
  const pv = reconcileProcessingVersion(state);
  if (pv.changed) {
    // biome-ignore lint/suspicious/noConsole: one-time visible event
    console.log(
      `[modelstat] processing pipeline v${pv.from} → v${pv.to} — wiped file cursors so every session is re-processed by the new pipeline`,
    );
  }

  await runDiscovery();
  await requestScan("startup");

  // Lazy-load chokidar to keep cold-start fast
  const chokidar = (await import("chokidar")).default;
  const { homedir, platform } = await import("node:os");
  const { join } = await import("node:path");
  const home = homedir();
  const dirs = [
    join(home, ".claude/projects"),
    join(home, ".codex/sessions"),
    join(home, ".cursor/ai-tracking"),
    join(home, ".gemini"),
    ...(platform() === "darwin"
      ? [
          join(home, "Library/Application Support/Cursor/User/workspaceStorage"),
          join(home, "Library/Application Support/Claude"),
        ]
      : [join(home, ".config/Cursor/User/workspaceStorage")]),
  ].filter((p) => existsSync(p) && statSync(p).isDirectory());

  setPhase("watching", `Watching ${dirs.length} directories`);

  const watcher = chokidar.watch(dirs, {
    persistent: true,
    ignoreInitial: true,
    depth: 10,
    awaitWriteFinish: { stabilityThreshold: 500, pollInterval: 200 },
  });

  let scanTimer: NodeJS.Timeout | null = null;
  const scheduleScan = (reason: string) => {
    if (scanTimer) return; // debounce
    scanTimer = setTimeout(() => {
      scanTimer = null;
      void requestScan(reason);
    }, 1_000);
  };

  watcher
    .on("add", (p) => {
      if (p.endsWith(".jsonl") || p.endsWith(".db")) scheduleScan(`add ${basename(p)}`);
    })
    .on("change", (p) => {
      if (p.endsWith(".jsonl") || p.endsWith(".db")) scheduleScan(`change ${basename(p)}`);
    })
    .on("error", (e) => {
      setMessage(`watcher error: ${(e as Error).message}`);
    });

  // Backstop: every 5min scan anyway (FSEvents can miss things). Routed
  // through requestScan so it coalesces instead of stacking a second
  // scan on top of one that's still running (the backfill OOM).
  const backstop = setInterval(() => void requestScan("interval"), SCAN_INTERVAL_MS);
  backstop.unref();

  // Periodic re-discovery so newly-added accounts (e.g. user just
  // signed into a second codex/gemini account, or the Claude
  // Keychain item appeared) show up on the dashboard without
  // restarting the daemon. The probe reads 4 small JSON files +
  // one Keychain query — cheap enough to do every minute.
  //
  // Without this, identities are enumerated ONCE at daemon boot
  // (line ~282 above) and the dashboard's "Accounts" panel stays
  // stuck on the snapshot taken at install time.
  const discoveryTimer = setInterval(() => void runDiscovery(), DISCOVERY_INTERVAL_MS);
  discoveryTimer.unref();

  // Handle Ctrl-C / service restarts
  const shutdown = async (): Promise<void> => {
    setPhase("offline", "Shutting down");
    await sendHeartbeat();
    await watcher.close();
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);

  // Keep the process alive
  await new Promise<void>(() => {});
}
