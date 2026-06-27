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
import { createHash } from "node:crypto";
import { existsSync, statSync } from "node:fs";
import { describeErrorWithCause } from "@modelstat/daemon-core/logger";
import type { DetectedIdentity, DetectedInstallation } from "@modelstat/core";
import { discover } from "@modelstat/parsers";
import { postHeartbeat } from "./api.js";
import { state } from "./config.js";
import { acquireDaemonLock, formatAge } from "./lock.js";
import { machineKey } from "./machine-key.js";
import { refreshSessionInsights } from "./insights.js";
import {
  drainLocalQueue,
  type LocalIngestReceiver,
  localQueueDepth,
  startLocalIngestReceiver,
} from "./receiver.js";
import { reconcileBackfill } from "./reconcile.js";
import { runtimeState } from "./runtime-state.js";
import { scanAll, scanSession } from "./scan.js";
import { createCoalescingRunner } from "./single-flight.js";
import { autoUpdateEnabled, clearUpgradeMarker, maybeAutoUpdate } from "./update.js";

// Substituted by tsup's `define` at build time (see tsup.config.ts).
// Replaces an older runtime parent-walk for package.json that broke
// once the bundle was copied to ~/.modelstat/bin/ (no sibling
// package.json), making the daemon report "daemon-unknown" on every
// upgrade. cli.ts and scan.ts use the same macro.
const DAEMON_VERSION: string =
  typeof __MODELSTAT_VERSION__ === "string" ? __MODELSTAT_VERSION__ : "daemon-dev";
const HEARTBEAT_INTERVAL_MS = 10_000;
const SCAN_INTERVAL_MS = 5 * 60 * 1000; // backstop periodic scan
/** Discovery now RIDES the heartbeat: the snapshot is attached only when it
 * changed, plus a periodic backstop so the server's installs/identities can't
 * go stale even if a probe momentarily misses something. */
const DISCOVERY_BACKSTOP_MS = 5 * 60 * 1000;
// When the LLM recovers after a degraded run, re-scan to upgrade extractive
// abstracts — but at most this often, so a flaky LLM (preflight passes, scans
// fail) can't re-scan the whole history on every restart.
const SUMMARISER_RECOVERY_MIN_INTERVAL_MS = 6 * 60 * 60_000;

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
  daemon_version?: string;
  /** Stable hardware machine key (see machine-key.ts). Sent so the
   * server can backfill `devices.machine_id` onto an already-enrolled
   * row that registered before machine_id existed — which is what lets
   * machine-key dedupe protect legacy devices without a re-register. */
  machine_id?: string;
  /** Server release verdict for this daemon, set from the heartbeat response
   * and mirrored to last-status.json for the tray/CLI. null ⇒ up to date. */
  update?: { verdict: string; latest: string | null } | null;
  /** Effective auto-update setting (env override or stored pref) — the tray
   * reads this to render its checkbox. */
  auto_update?: boolean;
  /** Discovery snapshot — installs + signed-in accounts. FOLDED INTO the
   * heartbeat (the standalone /v1/devices/discovery endpoint is gone). Attached
   * only when the snapshot changed (or on the periodic backstop), so a steady
   * state ships a tiny liveness body. */
  installations?: DetectedInstallation[];
  identities?: DetectedIdentity[];
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
  update: null as { verdict: string; latest: string | null } | null,
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
/** Record the server's release verdict (or clear it). Surfaced to the tray/CLI
 * via last-status.json; the action (auto-update / nudge) is in handleRelease. */
function setUpdate(u: { verdict: string; latest: string | null } | null): void {
  status.update = u;
  scheduleLocalFlush();
}

/** Snapshot the shared mutable `status` into the wire/heartbeat shape.
 * Used by both the network heartbeat and the local-file mirror so the
 * two never drift. The local mirror keeps a `device_id` for the tray;
 * the wire body sent to the server omits it (it's in the URL path). */
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
    daemon_version: DAEMON_VERSION,
    machine_id: machineKey(),
    update: status.update,
    auto_update: autoUpdateEnabled(),
  };
}

/* ─── Discovery folded into the heartbeat ──────────────────────────────────
 * The standalone POST /v1/devices/discovery is gone server-side. The daemon
 * now attaches its installs/identities snapshot to the heartbeat:
 *   • on the FIRST heartbeat after boot,
 *   • whenever the snapshot CHANGES (hash differs), and
 *   • at most every DISCOVERY_BACKSTOP_MS as a backstop (so a momentarily-
 *     incomplete probe can't pin a stale snapshot on the server forever).
 * A discover() failure is best-effort: it must NEVER block liveness — the
 * heartbeat still goes out, just without the discovery arrays this tick. */
let lastDiscoveryHash: string | null = null;
let lastDiscoveryAttachedAt = 0;

function hashDiscovery(d: { installations: DetectedInstallation[]; identities: DetectedIdentity[] }): string {
  // Stable JSON over the parts that matter; cheap SHA-256 to detect change.
  return createHash("sha256").update(JSON.stringify({ i: d.installations, a: d.identities })).digest("hex");
}

/**
 * Best-effort discovery snapshot for the heartbeat. Returns the
 * installations/identities arrays to attach, or `null` to send a bare liveness
 * heartbeat (unchanged snapshot, or a discover() failure). NEVER throws.
 */
async function discoverySnapshotForHeartbeat(): Promise<{
  installations: DetectedInstallation[];
  identities: DetectedIdentity[];
} | null> {
  let d: { installations: DetectedInstallation[]; identities: DetectedIdentity[] };
  try {
    d = await discover();
  } catch (e) {
    // Discovery is best-effort — a probe failure can't block liveness. Surface
    // it on the status line but still send the heartbeat (caller attaches null).
    setMessage(`discovery deferred: ${describeErrorWithCause(e)}`);
    return null;
  }
  status.stats["installations_detected"] = d.installations.length;
  status.stats["identities_detected"] = d.identities.length;
  const hash = hashDiscovery(d);
  const now = Date.now();
  const changed = hash !== lastDiscoveryHash;
  const backstopDue = now - lastDiscoveryAttachedAt >= DISCOVERY_BACKSTOP_MS;
  if (!changed && !backstopDue) return null; // steady state — bare heartbeat
  lastDiscoveryHash = hash;
  lastDiscoveryAttachedAt = now;
  return { installations: d.installations, identities: d.identities };
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

// Track the last verdict so a transition is logged once (heartbeat fires every
// 10s); maybeAutoUpdate self-dedups the action per (verdict, target).
let lastVerdict = "ok";
async function handleRelease(rel?: { verdict?: string; latest?: string | null }): Promise<void> {
  const verdict = rel?.verdict ?? "ok";
  const latest = rel?.latest ?? null;
  if (verdict === "ok") {
    if (status.update) setUpdate(null);
    lastVerdict = "ok";
    return;
  }
  setUpdate({ verdict, latest });
  if (verdict !== lastVerdict) {
    // biome-ignore lint/suspicious/noConsole: one-line release transition for the logs
    console.log(
      `[modelstat] release ${verdict}: this daemon ${DAEMON_VERSION}, latest ${latest ?? "?"}`,
    );
  }
  lastVerdict = verdict;
  // On the auto-update path, quiesce (stop scans + drain + free Metal) BEFORE
  // the install spawns, so the postinstall's SIGTERM finds us already off the
  // device and the replacement daemon doesn't race two processes onto Metal.
  const note = await maybeAutoUpdate(verdict, latest, quiesceSummariser);
  if (note) {
    // biome-ignore lint/suspicious/noConsole: one-time auto-update note for the logs
    console.log(`[modelstat] ${note}`);
  }
}

async function sendHeartbeat(): Promise<void> {
  const bearer = state.bearer;
  const deviceId = state.deviceId;
  if (!bearer || !deviceId) return; // pre-enrollment

  // The local-mirror snapshot carries device_id for the tray; the WIRE body
  // does NOT (device_id is in the URL path now), so strip it before sending.
  const local = snapshotBody();
  const { device_id: _omit, ...liveness } = local;

  // Fold discovery into the heartbeat — attach the snapshot only when it changed
  // (or on the periodic backstop). Best-effort: a discover() failure returns
  // null and the bare liveness heartbeat still goes out.
  const snap = await discoverySnapshotForHeartbeat();
  const wireBody: Record<string, unknown> = { ...liveness };
  if (snap) {
    wireBody.installations = snap.installations;
    wireBody.identities = snap.identities;
  }

  // Route through the shared device client: POST the path-style URL, get the
  // SAME 401→recoverIdentity + 5xx-backoff handling as every other device call.
  // postHeartbeat returns the unwrapped `.data` (or null on a non-recoverable
  // failure / network blip — in which case the dashboard sees a stale heartbeat
  // and the local phase flips via the scanner's catch on the next upload).
  try {
    const data = await postHeartbeat(deviceId, wireBody);
    // The server returns our release verdict (ok / update_available /
    // upgrade_required) — act on it (alert + auto-update).
    if (data?.daemon_release) await handleRelease(data.daemon_release);
  } catch {
    // postHeartbeat already absorbs network/HTTP errors into null; a throw here
    // would be unexpected — ignore so the next tick retries.
  }

  // Mirror the heartbeat to ~/.modelstat/last-status.json so the
  // tray + CLI (`modelstat status`/`jobs`) can read fresh numbers
  // without an authenticated round-trip to the server. This is now the
  // SOLE source of local usage numbers: the old public
  // /v1/device/:claim_code capability endpoint was removed server-side
  // (it returns the SPA HTML now), so the snapshot's phase / queue /
  // segment-upload stats are what `modelstat status --json` reports.
  writeLocalStatus(local).catch(() => undefined);
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
  const { join } = await import("node:path");
  const { writeFile, mkdir, rename } = await import("node:fs/promises");
  const { modelstatHome } = await import("./paths.js");
  if (!lastStatusPath) {
    const dir = modelstatHome();
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
      onDropped() {
        // The server permanently rejected a batch (400/422) and the scanner
        // quarantined it (logging the reason loudly) so newer data keeps flowing —
        // surface the COUNT so the (rare) daemon-side data loss is visible in the
        // tray, not silent. Don't flip phase: the daemon is healthy, it just
        // skipped one un-encodable batch.
        bumpStat("batches_dropped", 1);
        setStat("segments_sending", 0);
      },
    });
    bumpStat("files_scanned", r.filesScanned);
    bumpStat("files_unchanged", r.filesUnchanged);
    if (r.morePending) {
      // The cycle hit its file cap with older history still to drain — re-scan
      // promptly (newest-first, bounded per cycle) so a fresh device's backlog
      // fills in over quick successive scans instead of one giant, OOM-prone,
      // crash-looping pass.
      setPhase("processing", "Catching up on history…");
      setTimeout(() => void requestScan("backfill"), 250);
    } else {
      setPhase("watching", "Waiting for new events");
      setProgress(0, 0);
    }
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
// Set once we begin quiescing for a self-update (or shutdown). Once armed, no
// new scan is admitted — the process is about to be replaced/killed and we must
// not start fresh work on a Metal device we're about to free.
let quiescing = false;
function requestScan(reason: string): Promise<void> {
  if (quiescing) return Promise.resolve();
  return scanRunner.trigger(reason);
}

/**
 * Make the process safe to kill: stop admitting new scans, let the in-flight
 * scan drain, then free the bundled summariser's Metal device. Called before a
 * self-update spawns `npm i -g` (so the to-be-killed daemon is already off the
 * device when its replacement initialises Metal) and reusable by shutdown.
 * Idempotent + best-effort: never throws.
 */
async function quiesceSummariser(): Promise<void> {
  quiescing = true;
  try {
    await scanRunner.idle();
  } catch {
    /* idle() can't reject; never block on it */
  }
  try {
    const { disposeLlama } = await import("@modelstat/daemon-core/node");
    await disposeLlama();
  } catch {
    /* best-effort */
  }
}

/**
 * Eager single-session force-scan, invoked by the loopback control endpoint
 * (`POST /v1/control/scan` — see receiver.ts). A warm daemon already has the
 * summariser resident, so this lands a session you JUST finished in seconds,
 * then refreshes that session's server insights into the local cache the
 * statusline reads. Runs safely alongside the periodic file scan: the bundled
 * summariser serialises its own inference, and a force-scan still advances the
 * cursor so the next incremental `scanAll` stays consistent.
 *
 * The receiver already single-flights control scans, so this just does the
 * work + reports status; failures are surfaced to the caller (a `wait:true`
 * POST) but never crash the daemon.
 */
async function runEagerSessionScan(target: {
  sessionIds?: string[];
  file?: string;
}): Promise<void> {
  setPhase("scanning", "Eager scan (current session)");
  try {
    const r = await scanSession(target, {
      onProgress(p) {
        if (p.segment === 0) setPhase("processing", "Analyzing current session");
        else setPhase("processing", `Summarising segment ${p.segment}/${p.segmentTotal}`);
      },
      onUpload({ segments }) {
        setPhase("uploading", `Uploading ${segments} segments`);
        setStat("segments_sending", segments);
      },
      onUploaded({ events, segments }) {
        bumpStat("events_uploaded", events);
        setStat("segments_sent", state.bumpSegmentsSent(segments));
        setStat("segments_sending", 0);
        status.lastEventAt = new Date().toISOString();
      },
    });
    // Refresh the local insights cache for the scanned session(s) so the
    // statusline shows fresh numbers. Prefer the explicit chain; otherwise the
    // server resolves nothing useful, so only refresh when ids were given.
    if (target.sessionIds && target.sessionIds.length > 0) {
      await refreshSessionInsights(target.sessionIds);
    }
    setPhase("watching", "Waiting for new events");
    setMessage(
      r.segmentsUploaded > 0
        ? `Eager scan: ${r.segmentsUploaded} segments uploaded`
        : "Eager scan: nothing new",
    );
  } catch (e) {
    setStat("segments_sending", 0);
    setPhase("watching", `Eager scan failed: ${describeErrorWithCause(e)}`);
    throw e;
  }
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
    daemonVersion: DAEMON_VERSION,
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
      )} ago, daemon ${lock.owner.daemonVersion}.`,
    );
    // biome-ignore lint/suspicious/noConsole: user-visible CLI output
    console.log("  → to stop it:          kill " + lock.owner.pid);
    // biome-ignore lint/suspicious/noConsole: user-visible CLI output
    console.log("  → to force-replace it: modelstat start --force");
    return;
  }

  setPhase("starting", "Booting");
  // We hold the singleton lock, so we ARE the live daemon now — any in-flight
  // upgrade has landed (the postinstall stopped the old one and kickstarted us).
  // Clear the upgrade marker so a future verdict isn't suppressed by a stale one.
  clearUpgradeMarker();
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

  // Verify the summariser before producing segments. The daemon NO LONGER
  // refuses to start when the LLM can't load — that left users with zero data.
  // It degrades to the dependency-free extractive fallback (loud warning +
  // degraded status line) and self-heals once the LLM is healthy again.
  const wasDegraded = runtimeState.getSummariserDegraded();
  try {
    setPhase("starting", "Preflight: summariser");
    const { preflightSummariser } = await import("./pipeline.js");
    const { label, degraded } = await preflightSummariser();
    if (degraded) {
      // biome-ignore lint/suspicious/noConsole: loud degraded startup status
      console.warn(`[modelstat] ⚠ summariser preflight DEGRADED — ${label}`);
      setMessage(
        "summariser degraded: extractive fallback (LLM unavailable) — ingest continues, self-heals when the model loads",
      );
    } else {
      // biome-ignore lint/suspicious/noConsole: startup status
      console.log(`[modelstat] summariser preflight ok: ${label}`);
      // Self-heal: the LLM is healthy, but the LAST run shipped extractive
      // abstracts → re-scan so they upgrade to model quality. Rate-gated so a
      // flaky LLM can't re-scan the whole history on every restart.
      if (wasDegraded) {
        const since = Date.now() - runtimeState.getSummariserRecoveryAt();
        if (since > SUMMARISER_RECOVERY_MIN_INTERVAL_MS) {
          runtimeState.wipeCursors();
          runtimeState.setSummariserRecoveryAt(Date.now());
          // biome-ignore lint/suspicious/noConsole: recovery status
          console.log(
            "[modelstat] summariser recovered — re-scanning so extractive fallback abstracts upgrade to model quality",
          );
        }
      }
      runtimeState.setSummariserDegraded(false);
    }
  } catch (err) {
    // The preflight no longer throws on a missing LLM (it degrades). A throw
    // here is genuinely unexpected — log and CONTINUE rather than refuse to
    // start; ingest availability is the priority.
    // biome-ignore lint/suspicious/noConsole: unexpected-but-tolerated
    console.warn(`[modelstat] summariser preflight error (continuing): ${(err as Error).message}`);
    setMessage(`summariser preflight error (continuing): ${(err as Error).message}`);
  }

  // Local loopback ingest receiver — the server half of the SDKs'
  // `local_daemon` mode (sdks/{node,python,rust} default to
  // http://127.0.0.1:4319/v1/ingest). SDK captures land in a durable queue
  // and drain through the SAME pipeline + uploader as file scans, under this
  // device's secret — so the SDK ships no credentials and only redacted
  // segment abstracts leave the machine. Best-effort: a busy port just
  // disables this path (the file scan is the daemon's core duty).
  const localIngest: LocalIngestReceiver | null = await startLocalIngestReceiver({
    // Serve the loopback control endpoint so `modelstat sync --session` can
    // warm this running daemon (summariser already loaded) instead of
    // cold-spawning its own — and so the eager scan refreshes the local
    // insights cache the statusline reads.
    onControlScan: runEagerSessionScan,
  });
  const LOCAL_DRAIN_INTERVAL_MS = 5_000;
  let localDrainTimer: NodeJS.Timeout | null = null;
  if (localIngest) {
    // Secondary backoff: the 5s timer fires steadily, but after a failed drain we
    // SKIP an increasing number of ticks (capped) so a sustained backend outage
    // isn't retried every 5s for hours. uploadBatch already backs off WITHIN an
    // attempt; this spaces the attempts too (≈5s → up to ~30s while down). A
    // success (or an empty queue, which doesn't throw) resets it immediately.
    let drainFails = 0;
    let drainSkip = 0;
    const drainTick = async (): Promise<void> => {
      if (drainSkip > 0) {
        drainSkip--;
        return;
      }
      try {
        const { events } = await drainLocalQueue({
          deviceId: state.deviceId as string,
          daemonVersion: DAEMON_VERSION,
        });
        if (events > 0) bumpStat("sdk_events_uploaded", events);
        setStat("sdk_queue", await localQueueDepth());
        drainFails = 0;
      } catch (e) {
        // Backend unreachable: events stay durably queued and a later tick
        // retries. Surface it without flipping the whole daemon to "offline"
        // (the file-scan path owns that top-level signal).
        drainFails = Math.min(drainFails + 1, 6);
        drainSkip = drainFails;
        setMessage(`SDK ingest upload deferred: ${describeErrorWithCause(e)}`);
      }
    };
    localDrainTimer = setInterval(() => void drainTick(), LOCAL_DRAIN_INTERVAL_MS);
    localDrainTimer.unref();
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

  // Discovery now rides the heartbeat (the primed heartbeat above already
  // attaches the first snapshot, since lastDiscoveryHash starts null), so there
  // is no separate startup discovery pass any more. Go straight to scanning.
  await requestScan("startup");

  // Signed redaction-policy augment: fetch the additive `policies`
  // config, verify its Ed25519 signature against the bundled key, and union the
  // patterns over the local floor — refreshing on its own 15-min timer. Strictly
  // fail-safe: the compiled-in floor always applies, a forged/stale bundle is
  // rejected, and an offline boot degrades to floor-only. Never blocks startup.
  try {
    const { createPolicyRefresher } = await import("@modelstat/daemon-core/policies");
    await createPolicyRefresher({ apiUrl: state.apiUrl }).start();
  } catch (err) {
    setMessage(`policy refresh unavailable: ${(err as Error).message}`);
  }

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

  // Re-discovery is no longer a standalone timer/POST: the heartbeat (every
  // HEARTBEAT_INTERVAL_MS) re-runs discover() and attaches the snapshot only
  // when it CHANGES (newly-added account / fresh install) or on the
  // DISCOVERY_BACKSTOP_MS backstop — see discoverySnapshotForHeartbeat. So a
  // user who signs into a second codex/gemini account still shows up on the
  // dashboard within a heartbeat, without a separate discovery scheduler.

  // Self-healing reconcile: periodically verify the server still holds what we
  // shipped and re-ship precisely what it's missing (e.g. after a DB/raw-log
  // wipe). Cheap when in sync — one digest fetch + a summariser-free parse tally;
  // it only re-ships sessions the server is short on. The first pass runs shortly
  // after startup so a wiped scope refills on its own without a restart.
  const RECONCILE_INTERVAL_MS = 30 * 60_000;
  const reconcileTimer = setInterval(
    () => void reconcileBackfill(requestScan),
    RECONCILE_INTERVAL_MS,
  );
  reconcileTimer.unref();
  setTimeout(() => void reconcileBackfill(requestScan), 60_000).unref();

  // Handle Ctrl-C / service restarts
  let shuttingDown = false;
  const shutdown = async (): Promise<void> => {
    if (shuttingDown) return; // a second SIGTERM/SIGINT mustn't race the teardown
    shuttingDown = true;
    setPhase("offline", "Shutting down");
    await sendHeartbeat();
    // Stop admitting new file events before we drain — otherwise the watcher
    // could trigger a fresh scan while we're trying to quiesce.
    await watcher.close();
    if (localDrainTimer) clearInterval(localDrainTimer);
    await localIngest?.close();
    // Quiesce the scanner THEN free the summariser. A scan can be mid
    // segment-inference right now; disposing the Metal device underneath a live
    // llama context is exactly what aborted the process (`libc++abi … mutex lock
    // failed` / GGML_ASSERT) on every launchd stop/restart and on auto-update.
    // quiesceSummariser stops new scans, drains the in-flight one (idle()), then
    // disposeLlama frees contexts + model + device (with its own ≤8s drain cap).
    // No-op when the summariser was never loaded.
    await quiesceSummariser();
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);

  // Keep the process alive
  await new Promise<void>(() => {});
}
