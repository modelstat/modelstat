/**
 * Shared queue state machine.
 *
 * Both daemons feed captured events into an IngestQueue with a
 * runtime-specific QueueStore (SqliteQueueStore on Node, IdbQueueStore
 * in the extension). The state machine itself — dedupe, debouncing,
 * batching, retry — is implemented once here.
 *
 * This file intentionally ships the *types + tick function* only; the
 * store impls live in @modelstat/daemon-core/{node,browser} subpaths,
 * added in the step-5 follow-up of the unification migration.
 */
import type { IngestBatch, RawEvent, Segment, ToolCallWire } from "@modelstat/core/schemas";
import {
  FORCE_SHIP_THRESHOLD,
  INGEST_BATCH_MAX_EVENTS,
  SESSION_DEBOUNCE_MS,
} from "../config/index.js";
import { batchId } from "../ids.js";

/** Max per-call tool invocations per IngestBatch — mirrors the
 * `tool_calls: z.array(ToolCallWire).max(20_000)` cap on IngestBatch
 * (@modelstat/core/schemas). The server 400s (and the uploader drops)
 * any batch above it, so batch builders must split BEFORE the cap. */
export const INGEST_BATCH_MAX_TOOL_CALLS = 20_000;

/** A tool call before segment attribution — ToolCallWire minus
 * `segment_id`, which is only knowable once segments are built.
 * Structurally identical to ToolCallDraft in @modelstat/parsers
 * (defined there for parser output); duplicated as an Omit here so
 * daemon-core doesn't grow a dependency on the parsers package. */
export type ToolCallDraft = Omit<ToolCallWire, "segment_id">;

export interface QueueItem {
  /** Dedupe key — must match the upcoming event's `source_event_id`. */
  source_event_id: string;
  session_id: string;
  agent: string;
  event: RawEvent;
  last_event_ts_ms: number;
  synced: boolean;
  sent_batch_id: string | null;
  /** Per-call tool invocations born from `event` (same privacy
   * contract as ToolCallWire: hashes / sizes / allowlisted verbs
   * only). Optional — runtimes without tool-call data (the extension)
   * never set it, and old stored rows simply lack the key. */
  tool_calls?: ToolCallDraft[];
}

export interface QueueStore {
  put(item: QueueItem): Promise<void>;
  has(sourceEventId: string): Promise<boolean>;
  listUnsentBySession(): Promise<Map<string, QueueItem[]>>;
  markSent(sourceEventIds: string[], batchId: string | null): Promise<void>;
  countUnsent(): Promise<number>;
}

export interface PipelineRunner {
  /** Given a group of same-session events, return Segment[]. Phase-1
   * CLI pipeline will return a single heuristic segment here; later
   * phases plug in the real redact→tokenize→segment→summarise→tag
   * flow via @modelstat/daemon-core/pipeline. */
  run(events: RawEvent[]): Promise<Segment[]>;
}

/** One pass through the queue: gather unsent events per session, apply
 * debounce, run the pipeline, return batches ready for upload. Caller
 * (the daemon's tick loop) is responsible for actually uploading
 * and calling `markSent` on commit. */
export async function buildBatches(opts: {
  store: QueueStore;
  pipeline: PipelineRunner;
  deviceId: string;
  daemonVersion: string;
  nowMs: number;
  debounceMs?: number;
  maxEvents?: number;
  forceShipThreshold?: number;
}): Promise<IngestBatch[]> {
  const debounceMs = opts.debounceMs ?? SESSION_DEBOUNCE_MS;
  const maxEvents = opts.maxEvents ?? INGEST_BATCH_MAX_EVENTS;
  const forceThreshold = opts.forceShipThreshold ?? FORCE_SHIP_THRESHOLD;
  const bySession = await opts.store.listUnsentBySession();
  const batches: IngestBatch[] = [];
  let eventsInCurrentBatch: RawEvent[] = [];
  let sessionIdsInCurrentBatch: string[] = [];
  let toolCallsInCurrentBatch: ToolCallDraft[] = [];
  for (const [, items] of bySession) {
    if (items.length === 0) continue;
    // Debounce: skip sessions still producing events, unless over the
    // force-ship threshold.
    const lastTs = Math.max(...items.map((i) => i.last_event_ts_ms));
    const quiet = opts.nowMs - lastTs >= debounceMs;
    if (!quiet && items.length < forceThreshold) continue;

    for (const item of items) {
      const itemCalls = item.tool_calls ?? [];
      // Split BEFORE either cap overflows. A tool call always ships in
      // the same batch as its emitting event, so the tool-call cap
      // check must run pre-push (post-push splitting would leave a
      // straddling event's calls on the wrong side). The non-empty
      // guard keeps a pathological single event with >cap calls from
      // looping — it ships alone (the wire schema will reject it, but
      // that's a parser bug, not a batching one).
      if (
        eventsInCurrentBatch.length >= maxEvents ||
        (eventsInCurrentBatch.length > 0 &&
          toolCallsInCurrentBatch.length + itemCalls.length > INGEST_BATCH_MAX_TOOL_CALLS)
      ) {
        batches.push(
          await finalise({
            events: eventsInCurrentBatch,
            sessionIds: sessionIdsInCurrentBatch,
            toolCalls: toolCallsInCurrentBatch,
            pipeline: opts.pipeline,
            deviceId: opts.deviceId,
            daemonVersion: opts.daemonVersion,
          }),
        );
        eventsInCurrentBatch = [];
        sessionIdsInCurrentBatch = [];
        toolCallsInCurrentBatch = [];
      }
      eventsInCurrentBatch.push(item.event);
      sessionIdsInCurrentBatch.push(item.session_id);
      toolCallsInCurrentBatch.push(...itemCalls);
    }
  }
  if (eventsInCurrentBatch.length > 0) {
    batches.push(
      await finalise({
        events: eventsInCurrentBatch,
        sessionIds: sessionIdsInCurrentBatch,
        toolCalls: toolCallsInCurrentBatch,
        pipeline: opts.pipeline,
        deviceId: opts.deviceId,
        daemonVersion: opts.daemonVersion,
      }),
    );
  }
  return batches;
}

/**
 * Fill `segment_id` on tool-call drafts from the segments built over
 * the same events: a call whose `source_event_id` appears in a
 * segment's `source_event_ids` belongs to that segment. Calls whose
 * source event no segment covers (codex response_item anchors, slices
 * that failed to summarise) ship with `segment_id: null` — they stay
 * queryable by session server-side. Pure so both ship paths (agent
 * scan loop + this queue) share and test one implementation.
 */
export function attachSegmentIds(
  calls: readonly ToolCallDraft[],
  segments: readonly Segment[],
): ToolCallWire[] {
  const segmentByEvent = new Map<string, string>();
  for (const seg of segments) {
    for (const id of seg.source_event_ids) segmentByEvent.set(id, seg.segment_id);
  }
  return attachSegmentIdsByMap(calls, segmentByEvent);
}

/**
 * Same as {@link attachSegmentIds} but from a prebuilt `source_event_id →
 * segment_id` index. The streaming scan path needs this: a file whose
 * events straddle a batch-flush boundary has its early events (and their
 * segments) shipped in an earlier batch, while its tool-call drafts are
 * buffered at parse-end and ship later — so the call's segment is no
 * longer in the *current* batch's segment list. Resolving against an
 * index accumulated over every batch seen this run keeps those straddling
 * calls attributed instead of dropping to `segment_id: null`. (The queue
 * path doesn't need this — it keeps each call in the same batch as its
 * event — so it still calls {@link attachSegmentIds} directly.)
 */
export function attachSegmentIdsByMap(
  calls: readonly ToolCallDraft[],
  segmentByEvent: ReadonlyMap<string, string>,
): ToolCallWire[] {
  return calls.map((c) => ({
    ...c,
    segment_id: segmentByEvent.get(c.source_event_id) ?? null,
  }));
}

async function finalise(args: {
  events: RawEvent[];
  sessionIds: string[];
  toolCalls: ToolCallDraft[];
  pipeline: PipelineRunner;
  deviceId: string;
  daemonVersion: string;
}): Promise<IngestBatch> {
  // Pipeline runs per-session so segments don't span session boundaries.
  const segments: Segment[] = [];
  const bySession = new Map<string, RawEvent[]>();
  for (let i = 0; i < args.events.length; i++) {
    const sid = args.sessionIds[i] ?? args.events[i]?.session_id ?? "unknown";
    const arr = bySession.get(sid) ?? [];
    const ev = args.events[i];
    if (ev) arr.push(ev);
    bySession.set(sid, arr);
  }
  for (const [, sessionEvents] of bySession) {
    const segs = await args.pipeline.run(sessionEvents);
    segments.push(...segs);
  }
  return {
    batch_id: batchId(),
    device_id: args.deviceId,
    daemon_version: args.daemonVersion,
    events: args.events,
    segments,
    // Segments are in hand here, so segment attribution happens at the
    // last responsible moment before the wire.
    tool_calls: attachSegmentIds(args.toolCalls, segments),
  };
}
