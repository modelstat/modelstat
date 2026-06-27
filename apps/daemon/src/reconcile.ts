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

/**
 * Grace band so a server that DEDUPES a re-shipped session (its reported count
 * never reaches local) doesn't make us re-ship forever. Three guards, all
 * erring toward loss-PROOF — a small threshold + backoff, never suppression:
 *
 *   1. Settle window — skip days at/after this cutoff. The server ingests
 *      asynchronously, so "today" (and the last few hours) legitimately lags
 *      local; reconciling it would re-ship sessions that are merely in flight.
 *      We only act on calendar days STRICTLY OLDER than this, where a real
 *      deficit means genuine loss, not lag.
 *   2. Min deficit — ignore a per-(day,session) shortfall of ≤ this many events.
 *      Off-by-a-few counts come from benign skew (a tail event the server
 *      hasn't folded yet, an extractive-vs-model segment-count diff); re-shipping
 *      for them churns without ever converging.
 *   3. Per-session backoff — once we've re-shipped a (day,session) and the
 *      server is STILL short next pass, wait exponentially longer before trying
 *      again (cap MAX). A genuinely-missing session refills on the first attempt;
 *      a server-deduped one stops hammering after a couple of tries.
 */
const RESHIP_SETTLE_MS = 6 * 60 * 60_000; // 6h — events older than this should have settled server-side
const RESHIP_MIN_DEFICIT = 2; // ignore a shortfall of ≤2 events (benign skew)
const RESHIP_BACKOFF_BASE_MS = 30 * 60_000; // first retry no sooner than ~30min after a re-ship
const RESHIP_BACKOFF_MAX_MS = 24 * 60 * 60_000; // never wait longer than a day between retries

export interface ReconcileOutcome {
  /** True when the server already holds everything local — no re-ship. */
  inSync: boolean;
  localEvents: number;
  serverEvents: number;
  /** Files re-parsed this pass (0 in steady state — everything cache-hit). */
  filesParsed: number;
  /** Divergent days drilled into. */
  daysChecked: number;
  /** Sessions the server was short on AND that cleared the grace band (eligible
   * to re-ship this pass). */
  sessionsShort: number;
  /** Sessions short but HELD back this pass by the grace band (settle window,
   * sub-threshold deficit, or backoff) — counted so a deferral is observable,
   * not silent. */
  sessionsDeferred: number;
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
    sessionsDeferred: 0,
    filesInvalidated: 0,
  };
  if (localEvents <= serverDays.total_events) return base;

  // Settle cutoff: only reconcile calendar days STRICTLY OLDER than this. The
  // server ingests asynchronously, so recent days legitimately lag local — a
  // shortfall there is in-flight data, not loss. `utcDay` of (now − settle).
  const now = Date.now();
  const cutoffDay = utcDay(new Date(now - RESHIP_SETTLE_MS).toISOString());

  // 4. Drill only into days the server is short on AND old enough to have settled.
  const serverDayMap = new Map(serverDays.days.map((d) => [d.day, d.events]));
  const reship = runtimeState.getReshipState();
  const seenKeys = new Set<string>(); // (day,session) keys observed this pass — for GC
  const filesToReship = new Set<string>();
  let sessionsShort = 0;
  let sessionsDeferred = 0;
  let daysChecked = 0;
  for (const [day, localCount] of localDay) {
    if (localCount <= (serverDayMap.get(day) ?? 0)) continue; // day in sync
    if (day >= cutoffDay) {
      // Too recent to trust the digest (still settling server-side) — defer the
      // whole day to a later pass. Count its sessions as deferred so the
      // deferral is observable; no per-session digest fetch needed.
      sessionsDeferred += localDaySession.get(day)?.size ?? 0;
      continue;
    }
    daysChecked += 1;
    const serverSessions = await fetchBackfillDaySessions(day);
    const serverHave = new Map(
      (serverSessions?.sessions ?? []).map((s) => [s.session_id, s.events]),
    );
    for (const [sid, n] of localDaySession.get(day) ?? []) {
      const key = `${day}\0${sid}`;
      seenKeys.add(key);
      const deficit = n - (serverHave.get(sid) ?? 0);
      if (deficit <= 0) {
        // In sync — drop any stale backoff bookkeeping for it.
        if (reship[key]) delete reship[key];
        continue;
      }
      // Guard 2: ignore a tiny shortfall (benign skew, not loss).
      if (deficit <= RESHIP_MIN_DEFICIT) {
        sessionsDeferred += 1;
        continue;
      }
      // Guard 3: exponential backoff once we've already re-shipped this
      // (day,session) and the server is STILL short (i.e. it deduped). The
      // first time we see a session short there's no record, so it re-ships
      // immediately; subsequent passes wait 30min·2^(attempts-1) (capped 24h).
      const rec = reship[key];
      if (rec && rec.attempts > 0) {
        const wait = Math.min(
          RESHIP_BACKOFF_BASE_MS * 2 ** (rec.attempts - 1),
          RESHIP_BACKOFF_MAX_MS,
        );
        if (now - rec.lastAt < wait) {
          sessionsDeferred += 1;
          continue; // still inside the backoff window
        }
      }
      sessionsShort += 1;
      reship[key] = { attempts: (rec?.attempts ?? 0) + 1, lastAt: now };
      for (const f of filesOf.get(key) ?? []) filesToReship.add(f);
    }
  }
  // GC: drop backoff records for (day,session) pairs we no longer see as short
  // OR no longer have locally, so the map tracks the current short set, not
  // every session ever re-shipped.
  for (const key of Object.keys(reship)) {
    if (!seenKeys.has(key)) delete reship[key];
  }
  runtimeState.setReshipState(reship);

  if (filesToReship.size === 0) {
    return { ...base, daysChecked, sessionsDeferred };
  }
  for (const f of filesToReship) runtimeState.clearCursor(f);
  console.log(
    `[modelstat] self-heal: server short ${sessionsShort} session(s) across ${daysChecked} day(s) ` +
      `(${sessionsDeferred} deferred by grace band); re-shipping ${filesToReship.size} file(s) from local logs`,
  );
  await requestScan("self-heal");
  return {
    ...base,
    inSync: false,
    daysChecked,
    sessionsShort,
    sessionsDeferred,
    filesInvalidated: filesToReship.size,
  };
}
