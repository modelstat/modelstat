/**
 * Self-healing reconcile — bidirectional anti-entropy (specs/self-healing-ingest.md).
 *
 * The server is authoritative for what's ingested; the local cursor is only a
 * fast-path that goes stale when the server loses data. So we reconcile against
 * the server with a digest tree keyed on EVENTS (the cheap, deterministic layer
 * both sides hold without the summariser):
 *
 *   1. total — one number. Server holds everything ⇒ done (O(1) steady state).
 *   2. per-day — drill only into days the server is short on.
 *   3. per-session — within a short day, find the exact sessions; invalidate just
 *      those files' cursors so the normal scan re-ships them. Idempotent.
 *
 * The local digest is built INCREMENTALLY: a per-file (mtime → day → session →
 * count) cache means only files that CHANGED since last pass are re-parsed, so a
 * device with months of history reconciles in O(changed files), and the cache is
 * GC'd to the current file set so on-disk size stays flat.
 */
import { stat } from "node:fs/promises";
import type { RawEvent } from "@modelstat/core";
import { fetchBackfillDays, fetchBackfillDaySessions } from "./api.js";
import { state } from "./config.js";
import { runtimeState } from "./runtime-state.js";
import { discoverJobs } from "./scan.js";

/** day → session → event count. */
type PerDaySession = Record<string, Record<string, number>>;

export interface ReconcileOutcome {
  /** True when the server already holds everything local — no re-ship. */
  inSync: boolean;
  localEvents: number;
  serverEvents: number;
  /** Files re-parsed this pass (0 in steady state — everything cache-hit). */
  filesParsed: number;
  /** Divergent days drilled into. */
  daysChecked: number;
  /** Sessions the server was short on. */
  sessionsShort: number;
  /** Cursors invalidated so the next scan re-ships them. */
  filesInvalidated: number;
}

/** UTC calendar day of an event timestamp — matches the server's `toDate(ts)`. */
function utcDay(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? "" : d.toISOString().slice(0, 10);
}

/**
 * Run one reconcile pass. `null` when the daemon isn't enrolled or the server
 * digest is unreachable (no-op; the next pass retries).
 */
export async function reconcileBackfill(
  requestScan: (reason: string) => unknown,
): Promise<ReconcileOutcome | null> {
  const deviceId = state.deviceId;
  if (!deviceId) return null;

  // 1. Refresh the local digest cache incrementally — re-parse only files whose
  //    mtime changed; reuse cached counts for everything else.
  const jobs = await discoverJobs(deviceId);
  const present = new Set(jobs.map((j) => j.path));
  const cache = runtimeState.getReconcileCache();
  let filesParsed = 0;
  for (const job of jobs) {
    const mtime = (await stat(job.path).catch(() => null))?.mtimeMs ?? 0;
    const hit = cache[job.path];
    if (hit && hit.mtime === mtime) continue; // unchanged — reuse cached counts
    const perDaySession: PerDaySession = {};
    try {
      await job.parse(async (chunk: RawEvent[]) => {
        for (const e of chunk) {
          const bySession = (perDaySession[utcDay(e.ts)] ??= {});
          bySession[e.session_id] = (bySession[e.session_id] ?? 0) + 1;
        }
      });
      cache[job.path] = { mtime, perDaySession };
      filesParsed += 1;
    } catch (e) {
      console.warn(`  ! reconcile parse failed for ${job.path}:`, (e as Error).message);
    }
  }
  // GC: drop cache + cursor entries for files that no longer exist, so on-disk
  // state tracks the CURRENT file set rather than everything ever seen.
  for (const p of Object.keys(cache)) if (!present.has(p)) delete cache[p];
  runtimeState.setReconcileCache(cache);
  runtimeState.pruneCursors(present);

  // 2. Roll the cache up → local per-day totals, per-(day,session) counts, and
  //    the file(s) each (day,session) lives in (for precise re-ship).
  const localDay = new Map<string, number>();
  const localDaySession = new Map<string, Map<string, number>>();
  const filesOf = new Map<string, Set<string>>(); // `${day}\0${session}` → files
  let localEvents = 0;
  for (const [path, entry] of Object.entries(cache)) {
    for (const [day, sessions] of Object.entries(entry.perDaySession)) {
      let ds = localDaySession.get(day);
      if (!ds) {
        ds = new Map();
        localDaySession.set(day, ds);
      }
      for (const [sid, n] of Object.entries(sessions)) {
        localEvents += n;
        localDay.set(day, (localDay.get(day) ?? 0) + n);
        ds.set(sid, (ds.get(sid) ?? 0) + n);
        const key = `${day}\0${sid}`;
        let fs = filesOf.get(key);
        if (!fs) {
          fs = new Set();
          filesOf.set(key, fs);
        }
        fs.add(path);
      }
    }
  }

  // 3. Top of the tree: totals. The daemon only ADDS and the server digest is
  //    device-scoped, so `server >= local` ⇒ nothing to push.
  const serverDays = await fetchBackfillDays();
  if (!serverDays) return null;
  const base: ReconcileOutcome = {
    inSync: true,
    localEvents,
    serverEvents: serverDays.total_events,
    filesParsed,
    daysChecked: 0,
    sessionsShort: 0,
    filesInvalidated: 0,
  };
  if (localEvents <= serverDays.total_events) return base;

  // 4. Drill only into days the server is short on.
  const serverDayMap = new Map(serverDays.days.map((d) => [d.day, d.events]));
  const filesToReship = new Set<string>();
  let sessionsShort = 0;
  let daysChecked = 0;
  for (const [day, localCount] of localDay) {
    if (localCount <= (serverDayMap.get(day) ?? 0)) continue; // day in sync
    daysChecked += 1;
    const serverSessions = await fetchBackfillDaySessions(day);
    const have = new Map((serverSessions?.sessions ?? []).map((s) => [s.session_id, s.events]));
    for (const [sid, n] of localDaySession.get(day) ?? []) {
      if (n > (have.get(sid) ?? 0)) {
        sessionsShort += 1;
        for (const f of filesOf.get(`${day}\0${sid}`) ?? []) filesToReship.add(f);
      }
    }
  }

  if (filesToReship.size === 0) return { ...base, daysChecked };
  for (const f of filesToReship) runtimeState.clearCursor(f);
  console.log(
    `[modelstat] self-heal: server short ${sessionsShort} session(s) across ${daysChecked} day(s); ` +
      `re-shipping ${filesToReship.size} file(s) from local logs`,
  );
  await requestScan("self-heal");
  return {
    ...base,
    inSync: false,
    daysChecked,
    sessionsShort,
    filesInvalidated: filesToReship.size,
  };
}
