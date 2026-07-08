/**
 * One-shot scan: walk known Claude Code + Codex directories, parse each
 * JSONL, run the shared daemon pipeline (redact → segment →
 * summarise → tag), upload the resulting batch.
 *
 * Tracks per-file cursors in conf so a second scan only sends
 * incremental events.
 */
import { readdir, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import type { IngestBatch, RawEvent, Segment, SessionMetadata } from "@modelstat/core";
import { batchId } from "@modelstat/daemon-core";
import { INGEST_BATCH_MAX_EVENTS } from "@modelstat/daemon-core/config";
import type { SegmentProgressFn } from "@modelstat/daemon-core/pipeline";
import { attachSegmentIdsByMap, type ToolCallDraft } from "@modelstat/daemon-core/queue";
import {
  deriveSessionIdFromFilename,
  deriveSessionIdFromPiPath,
  deriveSessionIdFromRolloutPath,
  type LocalToolContext,
  type ParseResult,
  type ParserContext,
  parseClaudeCodeJsonl,
  parseCodexRollout,
  parsePiSession,
  quickChecksum,
} from "@modelstat/parsers";
import { uploadBatch } from "./api.js";
import { state } from "./config.js";
import { resolveAuthoritativeGit } from "./git-enrich.js";
import {
  buildSegments,
  buildSessionMetadata,
  buildSessionTitles,
  enrichScripts,
  prepareCloudRawEvents,
} from "./pipeline.js";

/** Substituted by tsup's `define` (see tsup.config.ts) — a string
 * literal in the bundle; falls back to "daemon-dev" when run unbundled
 * (tsx / tests), where the define isn't applied. */
const DAEMON_VERSION =
  typeof __MODELSTAT_VERSION__ === "string" ? __MODELSTAT_VERSION__ : "daemon-dev";
// Shared cross-daemon batch size — same value the extension uses, so
// both sides present an identical rate-limit profile to the ingest endpoint.
const BATCH_MAX_EVENTS = INGEST_BATCH_MAX_EVENTS;
/** Wire cap on IngestBatch.tool_calls (z.array(ToolCallWire).max(20_000)
 * in @modelstat/core/schemas) — the server 400s anything above it, so
 * the buffer must flush BEFORE crossing it. Never binding in practice
 * (one event batch × typical calls-per-event stays far below), but a
 * tool-storm session shouldn't be able to sink a whole upload. */
const BATCH_MAX_TOOL_CALLS = 20_000;

/** Regression guard on the in-flight event buffer. The scan loop's
 * memory contract is "at most one batch of events in memory at a
 * time": parsers stream events in small chunks (ParserContext.onEvents)
 * and the buffer flushes the moment it reaches BATCH_MAX_EVENTS, so it
 * can only briefly overshoot by less than one parser chunk. If a future
 * refactor reintroduces whole-file (or whole-corpus) accumulation —
 * the bug class behind the 2026-06-11 v1→v3 reprocess OOM — this trips
 * loudly on the first oversized file instead of dying hours in with a
 * V8 heap-limit crash. */
const BATCH_BUFFER_HARD_CAP = BATCH_MAX_EVENTS * 2;

/** A fully-zeroed TokenUsage. Our wire contract marks `RawEvent.tokens`
 * nullable (events with no usage — user turns, tool results — naturally
 * carry none), but the ingest server deserializes it as a required
 * struct and 400s the *entire batch* on the first null. Coerce null →
 * zeros at the wire boundary so one token-less event can't sink a whole
 * upload. Zeros are accurate: these events incur no billable tokens. */
const ZERO_TOKENS = {
  input: 0,
  output: 0,
  cache_creation: 0,
  cache_read: 0,
  reasoning: 0,
} as const;

function withNonNullTokens(e: RawEvent): RawEvent {
  return e.tokens ? e : { ...e, tokens: { ...ZERO_TOKENS } };
}

/** Per-batch tallies handed to the upload callbacks. `segments` is the
 * number of cognition segments built from the batch's events — the
 * unit the menu-bar tray surfaces as "sent". */
export interface BatchCounts {
  events: number;
  segments: number;
}

export interface ScanCallbacks {
  /** Called before we parse each file — index is 0-based, total is the
   * count of files discovered across all tools. */
  onFile?: (path: string, index: number, total: number) => void;
  /** Called right before we POST a batch to /v1/ingest — i.e. these
   * segments are now in-flight. */
  onUpload?: (counts: BatchCounts) => void;
  /** Called after the POST is confirmed accepted. `events` is the
   * server-accepted count; `segments` is what we sent. */
  onUploaded?: (counts: BatchCounts) => void;
  /** Called when a batch is PERMANENTLY rejected (400/422) and quarantined —
   * skipped so newer data keeps flowing. Surfaced loudly (it's daemon-side data
   * loss for an un-encodable batch); the tray/statusline can show a drop count. */
  onDropped?: (info: BatchCounts & { reason: string }) => void;
  /** Per-segment progress while a batch is being summarised — the slow,
   * previously-opaque phase. Forwarded straight from the pipeline so the
   * daemon can keep the heartbeat (and the tray) ticking. */
  onProgress?: SegmentProgressFn;
}

/** Streaming event sink: a parser pushes its file's events in small chunks. */
export type EventSink = (events: RawEvent[]) => Promise<void>;
/** A discovered transcript file + its streaming parser. */
export interface ScanJob {
  path: string;
  parse: (
    sink: EventSink,
  ) => Promise<{ toolCalls: ToolCallDraft[]; scriptContexts: LocalToolContext[] }>;
}

/**
 * Walk the known Claude Code + Codex transcript roots and return one
 * {@link ScanJob} per `.jsonl`. Shared by the incremental {@link scanAll} and the
 * self-healing reconcile, so both reason over the exact same file set.
 */
export async function discoverJobs(deviceId: string): Promise<ScanJob[]> {
  const jobs: ScanJob[] = [];

  // Claude Code
  try {
    const base = join(homedir(), ".claude/projects");
    const projects = await readdir(base).catch(() => []);
    for (const p of projects) {
      const dir = join(base, p);
      const ds = await stat(dir).catch(() => null);
      if (!ds?.isDirectory()) continue;
      const files = await readdir(dir);
      for (const f of files) {
        if (!f.endsWith(".jsonl")) continue;
        const full = join(dir, f);
        jobs.push({
          path: full,
          parse: async (sink) => {
            const r = await parseClaudeCodeJsonl({ deviceId, sourceFile: full, onEvents: sink });
            return { toolCalls: r.toolCalls ?? [], scriptContexts: r.scriptContexts ?? [] };
          },
        });
      }
    }
  } catch (e) {
    console.warn("claude scan skipped:", (e as Error).message);
  }

  // Codex
  try {
    const base = join(homedir(), ".codex/sessions");
    const years = await readdir(base).catch(() => []);
    for (const y of years) {
      const months = await readdir(join(base, y)).catch(() => []);
      for (const m of months) {
        const days = await readdir(join(base, y, m)).catch(() => []);
        for (const d of days) {
          const files = await readdir(join(base, y, m, d)).catch(() => []);
          for (const f of files) {
            if (!f.startsWith("rollout-") || !f.endsWith(".jsonl")) continue;
            const full = join(base, y, m, d, f);
            jobs.push({
              path: full,
              parse: async (sink) => {
                const r = await parseCodexRollout({ deviceId, sourceFile: full, onEvents: sink });
                return { toolCalls: r.toolCalls ?? [], scriptContexts: r.scriptContexts ?? [] };
              },
            });
          }
        }
      }
    }
  } catch (e) {
    console.warn("codex scan skipped:", (e as Error).message);
  }

  // pi — ~/.pi/agent/sessions/<encoded-cwd>/<TS>_<uuid>.jsonl
  try {
    const base = join(homedir(), ".pi/agent/sessions");
    const projects = await readdir(base).catch(() => []);
    for (const p of projects) {
      const dir = join(base, p);
      const ds = await stat(dir).catch(() => null);
      if (!ds?.isDirectory()) continue;
      const files = await readdir(dir);
      for (const f of files) {
        if (!f.endsWith(".jsonl")) continue;
        const full = join(dir, f);
        jobs.push({
          path: full,
          parse: async (sink) => {
            const r = await parsePiSession({ deviceId, sourceFile: full, onEvents: sink });
            return { toolCalls: r.toolCalls ?? [], scriptContexts: r.scriptContexts ?? [] };
          },
        });
      }
    }
  } catch (e) {
    console.warn("pi scan skipped:", (e as Error).message);
  }

  return jobs;
}

/** Tallies returned by every scan pass (full corpus or single session). */
export interface ScanResult {
  filesScanned: number;
  filesUnchanged: number;
  batchesUploaded: number;
  eventsUploaded: number;
  segmentsUploaded: number;
  /** Batches PERMANENTLY rejected (400/422) and quarantined this run — skipped so
   * the scan keeps flowing. Non-zero = daemon-side data loss worth investigating. */
  batchesDropped: number;
  /** True when the per-cycle file cap was hit and older files still remain —
   * the caller should re-scan promptly to drain the rest (newest-first).
   * Always false for an uncapped {@link scanSession}. */
  morePending: boolean;
}

/** Tuning knobs for {@link runScanOverJobs}, the shared parse→summarise→upload
 * loop behind both {@link scanAll} (incremental, capped) and
 * {@link scanSession} (force-read, uncapped). */
interface RunScanOptions {
  /** Cap on CHANGED files processed per call; `null` = no cap (single-session
   * force-scan). When the cap is hit with files still pending, the result's
   * `morePending` is set so the caller re-scans promptly. */
  maxFiles: number | null;
  /** Skip the per-file cursor unchanged-guard and re-read every job from byte
   * 0. The cursor is still ADVANCED after a confirmed upload, so a later
   * incremental scan stays correct. Used by {@link scanSession} so the current
   * session re-uploads even though the daemon already processed it. */
  forceReadAll: boolean;
}

// Cold-start / big-backfill bound: process at most this many CHANGED files
// per incremental scan cycle. A fresh device with thousands of old transcripts
// used to load the whole backlog, OOM, and die BEFORE the first upload ever
// landed — and cursors only advance on a CONFIRMED upload, so it crash-looped
// with zero progress. Capping + recent-first means the newest session lands
// fast, memory stays near the model's resident footprint, and the rest drains
// over quick follow-up cycles (see `morePending`).
const MAX_FILES_PER_SCAN = 12;

export async function scanAll(cb: ScanCallbacks = {}): Promise<ScanResult> {
  const deviceId = state.deviceId;
  if (!deviceId) throw new Error("daemon not enrolled — run `register` first");

  // Each job streams its file's events into the provided sink in small
  // chunks (ParserContext.onEvents) instead of returning them all at
  // once — a single multi-hundred-MB transcript must never materialise
  // as one array. The sink flushes batches as it fills, so a full
  // reprocess after a cursor wipe holds at most ~one batch in memory.
  // Per-call tool invocations (ToolCallDraft) are NOT streamed — they're
  // hash/byte metadata only (tiny), so the parser returns the whole
  // file's set, which the caller drains into the event stream below.
  const jobs = await discoverJobs(deviceId);

  // Recent-first: newest transcripts upload first, so a session you JUST
  // finished shows up within seconds instead of waiting behind a backlog of
  // old ones (readdir order is arbitrary).
  const ordered = await orderJobsNewestFirst(jobs);

  return runScanOverJobs(
    ordered,
    deviceId,
    { maxFiles: MAX_FILES_PER_SCAN, forceReadAll: false },
    cb,
  );
}

/**
 * Force-scan ONE session's transcript(s) immediately — the eager path behind
 * `modelstat sync --session` and the loopback control endpoint. A warm daemon
 * (summariser already resident) can land a session you JUST finished in
 * seconds, instead of waiting for the watch/backstop cycle.
 *
 * Targeting:
 *   - `sessionIds` — map each id to its `<id>.jsonl` (Claude Code) or
 *     `rollout-…-<id>.jsonl` (Codex) among the discovered jobs. A compaction
 *     chain is just several ids; pass them all.
 *   - `file` — scan one explicit transcript path (already validated by the
 *     caller as living under a known transcript root).
 *   - neither — fall back to the single newest `.jsonl` across all roots, so a
 *     bare `modelstat sync --session` still does the useful thing.
 *
 * Unlike {@link scanAll} this BYPASSES the per-file cursor unchanged-skip
 * (the daemon may have already uploaded this session) but still advances the
 * cursor after a confirmed upload — so the regular incremental scan stays
 * consistent. Uncapped: a single session is bounded work.
 */
export async function scanSession(
  target: { sessionIds?: string[]; file?: string } = {},
  cb: ScanCallbacks = {},
): Promise<ScanResult> {
  const deviceId = state.deviceId;
  if (!deviceId) throw new Error("daemon not enrolled — run `register` first");

  const all = await discoverJobs(deviceId);
  let selected: ScanJob[];

  if (target.file) {
    // Caller validated the path is under a known transcript root. Reuse the
    // discovered job if we already enumerated it (keeps its parser wiring);
    // otherwise build one ad-hoc so an explicit --file still works even when
    // discovery didn't surface it (e.g. a brand-new file mid-write).
    const existing = all.find((j) => j.path === target.file);
    selected = existing ? [existing] : [jobForFile(deviceId, target.file)];
  } else if (target.sessionIds && target.sessionIds.length > 0) {
    const wanted = new Set(target.sessionIds);
    selected = all.filter((j) => {
      const sid = sessionIdForPath(j.path);
      return sid != null && wanted.has(sid);
    });
  } else {
    // No target → newest transcript across all roots, so a bare invocation is
    // still useful (mirrors scanAll's recent-first intent at size 1).
    const ordered = await orderJobsNewestFirst(all);
    selected = ordered.slice(0, 1);
  }

  if (selected.length === 0) {
    return {
      filesScanned: 0,
      filesUnchanged: 0,
      batchesUploaded: 0,
      eventsUploaded: 0,
      segmentsUploaded: 0,
      batchesDropped: 0,
      morePending: false,
    };
  }

  return runScanOverJobs(selected, deviceId, { maxFiles: null, forceReadAll: true }, cb);
}

/** Map a discovered transcript path back to its session id — Claude Code's
 * `<uuid>.jsonl` or Codex's `rollout-…-<uuid>.jsonl`. Null when neither shape
 * matches (so it's never confused with a real id). */
function sessionIdForPath(path: string): string | null {
  if (path.includes("/.pi/agent/sessions/")) return deriveSessionIdFromPiPath(path);
  return deriveSessionIdFromFilename(path) ?? deriveSessionIdFromRolloutPath(path);
}

/** Build a one-off {@link ScanJob} for an explicit transcript path, picking
 * the parser from the directory shape (Codex rollouts live under
 * `.codex/sessions`; everything else is Claude Code JSONL). */
function parserForFile(full: string): (ctx: ParserContext) => Promise<ParseResult> {
  if (full.includes("/.codex/sessions/") && /rollout-.*\.jsonl$/.test(full)) {
    return parseCodexRollout;
  }
  if (full.includes("/.pi/agent/sessions/")) return parsePiSession;
  return parseClaudeCodeJsonl;
}

function jobForFile(deviceId: string, full: string): ScanJob {
  const parse = parserForFile(full);
  return {
    path: full,
    parse: async (sink) => {
      const r = await parse({ deviceId, sourceFile: full, onEvents: sink });
      return { toolCalls: r.toolCalls ?? [], scriptContexts: r.scriptContexts ?? [] };
    },
  };
}

/** Sort jobs newest-first by mtime (arbitrary readdir order otherwise). Stats
 * run in parallel — cheap. */
async function orderJobsNewestFirst(jobs: ScanJob[]): Promise<ScanJob[]> {
  return (
    await Promise.all(
      jobs.map(async (j) => ({
        job: j,
        mtimeMs: (await stat(j.path).catch(() => null))?.mtimeMs ?? 0,
      })),
    )
  )
    .sort((a, b) => b.mtimeMs - a.mtimeMs)
    .map((x) => x.job);
}

/**
 * Shared parse → summarise → upload loop over an ordered job list. Holds at
 * most ~one batch of events in memory (the streaming sink applies backpressure
 * to the file read) and advances each file's cursor only after its events land
 * — so a mid-scan failure re-tries the same events next run (idempotent
 * server-side). Both {@link scanAll} and {@link scanSession} funnel through
 * here; their only differences are {@link RunScanOptions} (file cap +
 * force-read).
 */
async function runScanOverJobs(
  ordered: ScanJob[],
  deviceId: string,
  opts: RunScanOptions,
  cb: ScanCallbacks,
): Promise<ScanResult> {
  let morePending = false;

  let filesScanned = 0;
  let filesUnchanged = 0;
  let batchesUploaded = 0;
  let eventsUploaded = 0;
  let segmentsUploaded = 0;
  let batchesDropped = 0;

  let buffer: RawEvent[] = [];
  // Per-call tool invocations travelling with the events in `buffer`.
  // A call always ships in the same batch as its emitting event so the
  // server can resolve segment/session context in one pass.
  let toolCallBuffer: ToolCallDraft[] = [];
  // Files whose events are in the current buffer but whose cursor has
  // NOT been advanced yet. Cursor advances only when the batch
  // containing their events has been confirmed uploaded — so a
  // mid-scan network failure means the next run re-parses the same
  // files and tries again (idempotent from the daemon's side).
  let pendingCursors: Array<{ path: string; cs: Awaited<ReturnType<typeof quickChecksum>> }> = [];
  // Segments accumulated per session across the whole run. A big file
  // can split a session across BATCH_MAX_EVENTS boundaries; the title
  // for a session must be computed from every segment seen so far in
  // this run, not just the current batch's tail — otherwise the later
  // batch's (partial-view) title would overwrite the better one
  // server-side (titles are last-write-wins per session).
  const runSegmentsBySession = new Map<string, Segment[]>();

  /** Ship one assembled batch and, on success, advance the buffered files'
   * cursors. Shared by the normal (abstracts → /v1/ingest) and cloud (raw turns
   * → /v1/ingest/raw) paths — `raw` picks the endpoint, `segmentCount` is 0 for
   * the cloud path (the server builds the segments). */
  async function commitBatch(
    batch: IngestBatch,
    o: { raw: boolean; segmentCount: number },
  ): Promise<void> {
    const eventCount = batch.events.length;
    // These records are now in-flight.
    cb.onUpload?.({ events: eventCount, segments: o.segmentCount });
    const res = await uploadBatch(batch, { raw: o.raw });
    if (!res.committed) {
      // PERMANENT server reject (400/422 — an un-encodable batch the server will
      // never accept). QUARANTINE it: advance these files' cursors PAST the poison
      // batch so the newest-first scan isn't wedged behind it forever (one bad
      // batch was blocking every newer file's events). A TRANSIENT failure would
      // have thrown above instead — held + retried — so good data is never lost on
      // a blip; only a genuinely-rejected batch is skipped, and loudly.
      batchesDropped += 1;
      console.error(
        `dropped ${eventCount} event(s) / ${o.segmentCount} segment(s) — server rejected the batch (${res.reason}); skipping so newer data keeps flowing`,
      );
      cb.onDropped?.({ events: eventCount, segments: o.segmentCount, reason: res.reason });
      for (const pc of pendingCursors) state.setCursor(pc.path, pc.cs);
      pendingCursors = [];
      buffer = [];
      toolCallBuffer = [];
      return;
    }
    batchesUploaded += 1;
    eventsUploaded += res.accepted;
    segmentsUploaded += o.segmentCount;
    // Upload confirmed — persist cursors for the files whose events
    // just landed. Batch upserts are server-side idempotent (segments
    // keyed by segment_id, sessions keyed by device+tool+source_session_id)
    // so even if we crash between here and the next scan, re-sending
    // the same events is safe.
    for (const pc of pendingCursors) state.setCursor(pc.path, pc.cs);
    pendingCursors = [];
    buffer = [];
    toolCallBuffer = [];
    cb.onUploaded?.({ events: res.accepted, segments: o.segmentCount });
  }

  async function flushBatch(): Promise<void> {
    if (!buffer.length && !toolCallBuffer.length) return;
    // Correct each event's repo identity to the AUTHORITATIVE git remote on
    // disk BEFORE segmentation, so the `projects` hint (→ "Spend by Repo")
    // keys on the real owner/repo instead of a path-guessed subdirectory such
    // as `accounting/controllers`. Also normalise token-less events to zeroed
    // TokenUsage — the ingest server rejects null tokens (see ZERO_TOKENS).
    // This runs in EVERY mode: cloud ships these events (with corrected git) to
    // /v1/ingest/raw, so the authoritative repo identity must be applied first.
    const events = await resolveAuthoritativeGit(buffer.map(withNonNullTokens));

    // ── Cloud mode: summarise server-side ────────────────────────────────
    // No local model runs. Redact the turns fully on-device (the parser's
    // regex floor + the on-device NER/PII pass; see prepareCloudRawEvents),
    // then ship them to /v1/ingest/raw with NO local segments — modelstat's
    // cloud summarises them. FAIL-CLOSED: if the NER redactor is unavailable
    // we must not ship raw content with floor-only redaction, so we fall
    // through to the normal path and ship LOCAL extractive abstracts (no raw
    // egress) instead.
    if (state.summarizerMode === "cloud") {
      const cloudEvents = await prepareCloudRawEvents(events, toolCallBuffer);
      if (cloudEvents) {
        await commitBatch(
          {
            batch_id: batchId(),
            device_id: deviceId,
            daemon_version: DAEMON_VERSION,
            events: cloudEvents,
            segments: [],
            // No local segments to attribute calls to — a null segment_id is
            // valid wire; the server attaches them when it segments the turns.
            tool_calls: attachSegmentIdsByMap(toolCallBuffer, new Map<string, string>()),
          },
          { raw: true, segmentCount: 0 },
        );
        return;
      }
      console.warn(
        "[modelstat] ⚠ cloud summariser HELD — the on-device NER/PII redactor " +
          "(@huggingface/transformers) isn't available, so turns can't be scrubbed before " +
          "leaving the machine. Shipping LOCAL extractive abstracts (no raw egress) instead. " +
          "Install @huggingface/transformers to enable cloud mode.",
      );
      // fall through to the normal (local extractive) path
    }
    // Shared pipeline produces one-or-more Segments per session:
    // redact → segment (time + embedding boundaries) → summarise → tag.
    // Adapter config is set once at process boot in cli.ts. Build first
    // so the callbacks can report how many *segments* (not just events)
    // are going up — that's the unit the tray surfaces.
    const segments: Segment[] = await buildSegments(events, cb.onProgress);
    // Title every session that has segments in THIS batch, using the
    // full set of its segments seen this run. One short local-model
    // call per session; deterministic fallback inside means a titler
    // hiccup can't block the upload.
    for (const seg of segments) {
      const arr = runSegmentsBySession.get(seg.session_id) ?? [];
      // Drop the 384-float `abstract_embedding` from the RUN-LONG accumulator:
      // titling, metadata, and call-attribution below only read the abstract +
      // ids, and this Map is held for the WHOLE scan. On a full re-scan (a
      // PROCESSING_VERSION bump wipes cursors) of a power-user's months of
      // history, retaining an embedding per segment grew the heap unbounded —
      // the 8GB-OOM crash-loop. The wire `segments` array (shipped this batch)
      // still carries the embedding; only this auxiliary copy sheds it.
      arr.push(
        seg.abstract_embedding === undefined ? seg : { ...seg, abstract_embedding: undefined },
      );
      runSegmentsBySession.set(seg.session_id, arr);
    }
    const titleInput: Segment[] = [];
    for (const sessionId of new Set(segments.map((s) => s.session_id))) {
      titleInput.push(...(runSegmentsBySession.get(sessionId) ?? []));
    }
    let sessionTitles: Record<string, string> = {};
    try {
      sessionTitles = await buildSessionTitles(titleInput);
    } catch (e) {
      // Titles are auxiliary — never sink a batch over them.
      console.warn("session titling failed — shipping batch untitled:", (e as Error).message);
    }
    // Per-session repo/PR/commit/issue metadata — the join layer between AI
    // spend and shipped work. Uses the run-accumulated segments (full session
    // view, like titles) for abstract scanning + this batch's events for git
    // context. Auxiliary + best-effort: a detection hiccup never sinks a batch.
    let sessionMetadata: Record<string, SessionMetadata> = {};
    try {
      sessionMetadata = await buildSessionMetadata(titleInput, events);
    } catch (e) {
      console.warn(
        "session metadata detection failed — shipping batch without it:",
        (e as Error).message,
      );
    }
    // Attribute each buffered call to the segment covering its source
    // event — resolved against EVERY segment seen this run for the
    // call's session, not just this batch's. A file whose events
    // straddle a BATCH_MAX_EVENTS flush ships its early events (and
    // their segments) in an earlier batch, while its tool-call drafts
    // buffer at parse-end and ride a later batch; matching only the
    // current batch's segments would drop those straddling calls to
    // segment_id null. runSegmentsBySession already accumulates these
    // segments per session (for titling, populated just above), so
    // reusing them costs no extra memory. A call whose event no segment
    // covers (codex response_item anchors, slices that failed to
    // summarise) still ships segment_id null — valid wire.
    const callSegmentByEvent = new Map<string, string>();
    for (const sessionId of new Set(toolCallBuffer.map((c) => c.session_id))) {
      for (const seg of runSegmentsBySession.get(sessionId) ?? []) {
        for (const id of seg.source_event_ids) callSegmentByEvent.set(id, seg.segment_id);
      }
    }
    const batch: IngestBatch = {
      batch_id: batchId(),
      device_id: deviceId,
      daemon_version: DAEMON_VERSION,
      events,
      segments,
      tool_calls: attachSegmentIdsByMap(toolCallBuffer, callSegmentByEvent),
      ...(Object.keys(sessionTitles).length ? { session_titles: sessionTitles } : {}),
      ...(Object.keys(sessionMetadata).length ? { session_metadata: sessionMetadata } : {}),
    };
    await commitBatch(batch, { raw: false, segmentCount: segments.length });
  }

  // Streaming sink shared by every job: parsers call this with small
  // chunks as they read, we flush whenever a full batch accumulates.
  // The `await flushBatch()` inside applies backpressure all the way
  // down to the file read — the parser doesn't read ahead while a
  // batch is being summarised/uploaded.
  const sink = async (events: RawEvent[]): Promise<void> => {
    for (const e of events) {
      buffer.push(e);
      if (buffer.length >= BATCH_MAX_EVENTS) await flushBatch();
    }
    if (buffer.length > BATCH_BUFFER_HARD_CAP) {
      throw new Error(
        `scan event buffer exceeded ${BATCH_BUFFER_HARD_CAP} events — incremental batch flushing has regressed (see BATCH_BUFFER_HARD_CAP)`,
      );
    }
  };
  // Buffer a file's tool-call drafts once parsing finishes. Drafts are
  // buffered here (not streamed with their events) on purpose: a draft's
  // status/latency/result-size are filled in-place when its tool_result
  // line is later paired, so it isn't final until the whole file is
  // parsed. Segment attribution is resolved at flush time against the
  // run-accumulated segments (see flushBatch), so a draft riding a later
  // batch than its emitting event still keeps its segment_id — no
  // same-batch affinity required.
  const bufferToolCalls = async (calls: readonly ToolCallDraft[]): Promise<void> => {
    for (const c of calls) {
      if (toolCallBuffer.length >= BATCH_MAX_TOOL_CALLS) await flushBatch();
      toolCallBuffer.push(c);
    }
  };

  for (let i = 0; i < ordered.length; i++) {
    const job = ordered[i]!;
    cb.onFile?.(job.path, i, ordered.length);
    const cur = state.getCursor(job.path);
    const cs = await quickChecksum(job.path).catch(() => null);
    // Incremental scans skip a file whose size+tail are unchanged since the
    // last upload. The eager single-session path (forceReadAll) bypasses this
    // so a session the daemon already processed still re-uploads — the cursor
    // is still advanced below, keeping later incremental scans consistent.
    if (!opts.forceReadAll && cs && cur && cur.size === cs.size && cur.tailHash === cs.tailHash) {
      filesUnchanged += 1;
      continue;
    }
    filesScanned += 1;
    try {
      // Events stream through `sink` (bounded memory); the file's per-call
      // tool drafts come back at the end and join the current batch's
      // tool-call buffer, alongside the events they were extracted from.
      const r = await job.parse(sink);
      // Summarise the script/bash FILES each command ran, on-device, into the
      // drafts' redacted ToolAction.scripts. Best-effort + additive:
      // failures leave the abstracts empty, never blocking the upload. The raw
      // command + cwd it needs ride r.scriptContexts (local-only, never shipped).
      try {
        await enrichScripts(r.toolCalls, r.scriptContexts ?? []);
      } catch (e) {
        console.warn(
          `  ! script-summary enrichment skipped for ${job.path}:`,
          (e as Error).message,
        );
      }
      await bufferToolCalls(r.toolCalls);
      // Queue the cursor advance — it'll be applied by the next
      // successful flushBatch(), not before. If we're offline / the
      // ingest fails, the next scan retries the same events from the
      // same cursor position.
      if (cs) pendingCursors.push({ path: job.path, cs });
    } catch (e) {
      console.warn(`  ! parse failed for ${job.path}:`, (e as Error).message);
    }
    // Stop after the cap so memory + time per cycle stay bounded. The trailing
    // flush below uploads what's buffered (advancing those files' cursors), and
    // `morePending` tells the daemon to re-scan the next newest batch. A null
    // cap (single-session force-scan) never breaks early — it's bounded work.
    if (opts.maxFiles !== null && filesScanned >= opts.maxFiles) {
      morePending = true;
      break;
    }
  }
  await flushBatch();

  return {
    filesScanned,
    filesUnchanged,
    batchesUploaded,
    eventsUploaded,
    segmentsUploaded,
    batchesDropped,
    morePending,
  };
}
