/**
 * One-shot scan: walk known Claude Code + Codex directories, parse each
 * JSONL, run the shared companion pipeline (redact → segment →
 * summarise → tag), upload the resulting batch.
 *
 * Tracks per-file cursors in conf so a second scan only sends
 * incremental events.
 */
import { readdir, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { batchId } from "@modelstat/companion-core";
import { INGEST_BATCH_MAX_EVENTS } from "@modelstat/companion-core/config";
import type { SegmentProgressFn } from "@modelstat/companion-core/pipeline";
import { attachSegmentIdsByMap, type ToolCallDraft } from "@modelstat/companion-core/queue";
import type { IngestBatch, RawEvent, Segment, SessionMetadata } from "@modelstat/core";
import {
  type LocalToolContext,
  parseClaudeCodeJsonl,
  parseCodexRollout,
  quickChecksum,
} from "@modelstat/parsers";
import { uploadBatch } from "./api.js";
import { state } from "./config.js";
import {
  buildSegments,
  buildSessionMetadata,
  buildSessionTitles,
  enrichScripts,
} from "./pipeline.js";

/** Substituted by tsup's `define` (see tsup.config.ts) — a string
 * literal in the bundle; falls back to "daemon-dev" when run unbundled
 * (tsx / tests), where the define isn't applied. */
const DAEMON_VERSION =
  typeof __MODELSTAT_VERSION__ === "string" ? __MODELSTAT_VERSION__ : "daemon-dev";
// Shared cross-companion batch size — same value the extension uses, so
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
  /** Per-segment progress while a batch is being summarised — the slow,
   * previously-opaque phase. Forwarded straight from the pipeline so the
   * daemon can keep the heartbeat (and the tray) ticking. */
  onProgress?: SegmentProgressFn;
}

export async function scanAll(cb: ScanCallbacks = {}): Promise<{
  filesScanned: number;
  filesUnchanged: number;
  batchesUploaded: number;
  eventsUploaded: number;
  segmentsUploaded: number;
  /** True when the per-cycle file cap was hit and older files still remain —
   * the caller should re-scan promptly to drain the rest (newest-first). */
  morePending: boolean;
}> {
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
  type EventSink = (events: RawEvent[]) => Promise<void>;
  const jobs: Array<{
    parse: (
      sink: EventSink,
    ) => Promise<{ toolCalls: ToolCallDraft[]; scriptContexts: LocalToolContext[] }>;
    path: string;
  }> = [];

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

  // Recent-first: newest transcripts upload first, so a session you JUST
  // finished shows up within seconds instead of waiting behind a backlog of
  // old ones (readdir order is arbitrary). Stats run in parallel — cheap.
  const ordered = (
    await Promise.all(
      jobs.map(async (j) => ({
        job: j,
        mtimeMs: (await stat(j.path).catch(() => null))?.mtimeMs ?? 0,
      })),
    )
  )
    .sort((a, b) => b.mtimeMs - a.mtimeMs)
    .map((x) => x.job);

  // Cold-start / big-backfill bound: process at most this many CHANGED files
  // per scan cycle. A fresh device with thousands of old transcripts used to
  // load the whole backlog, OOM, and die BEFORE the first upload ever landed —
  // and cursors only advance on a CONFIRMED upload, so it crash-looped with
  // zero progress. Capping + recent-first means the newest session lands fast,
  // memory stays near the model's resident footprint, and the rest drains over
  // quick follow-up cycles (see `morePending`).
  const MAX_FILES_PER_SCAN = 12;
  let morePending = false;

  let filesScanned = 0;
  let filesUnchanged = 0;
  let batchesUploaded = 0;
  let eventsUploaded = 0;
  let segmentsUploaded = 0;

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

  async function flushBatch(): Promise<void> {
    if (!buffer.length && !toolCallBuffer.length) return;
    // Normalise token-less events to zeroed TokenUsage before they hit
    // the wire — the ingest server rejects null tokens (see ZERO_TOKENS).
    const events = buffer.map(withNonNullTokens);
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
      arr.push(seg);
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
      device_id: deviceId!,
      companion_version: DAEMON_VERSION,
      events,
      segments,
      tool_calls: attachSegmentIdsByMap(toolCallBuffer, callSegmentByEvent),
      ...(Object.keys(sessionTitles).length ? { session_titles: sessionTitles } : {}),
      ...(Object.keys(sessionMetadata).length ? { session_metadata: sessionMetadata } : {}),
    };
    // These segments are now in-flight.
    cb.onUpload?.({ events: events.length, segments: segments.length });
    const res = await uploadBatch(batch);
    batchesUploaded += 1;
    eventsUploaded += res.accepted;
    segmentsUploaded += segments.length;
    // Upload confirmed — persist cursors for the files whose events
    // just landed. Batch upserts are server-side idempotent (segments
    // keyed by segment_id, sessions keyed by device+tool+source_session_id)
    // so even if we crash between here and the next scan, re-sending
    // the same events is safe.
    for (const pc of pendingCursors) state.setCursor(pc.path, pc.cs);
    pendingCursors = [];
    buffer = [];
    toolCallBuffer = [];
    cb.onUploaded?.({ events: res.accepted, segments: segments.length });
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
    if (cs && cur && cur.size === cs.size && cur.tailHash === cs.tailHash) {
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
    // `morePending` tells the daemon to re-scan the next newest batch.
    if (filesScanned >= MAX_FILES_PER_SCAN) {
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
    morePending,
  };
}
