import assert from "node:assert/strict";
import { describe, it } from "node:test";
import type { RawEvent, Segment } from "@modelstat/core/schemas";
import {
  attachSegmentIds,
  attachSegmentIdsByMap,
  buildBatches,
  INGEST_BATCH_MAX_TOOL_CALLS,
  type PipelineRunner,
  type QueueItem,
  type QueueStore,
  type ToolCallDraft,
} from "./index.js";

const TS = "2026-06-01T10:00:00.000Z";

function rawEvent(
  over: Partial<RawEvent> & { source_event_id: string; session_id: string },
): RawEvent {
  return {
    ts: TS,
    kind: "assistant_message",
    agent: "claude_code",
    provider: "anthropic",
    model: "claude-fable-5",
    turn_index: null,
    parent_event_id: null,
    cwd: null,
    git: null,
    tokens: null,
    duration_ms: null,
    tool_calls: {},
    files_touched: [],
    source_file: null,
    source_byte_offset: null,
    ...over,
  };
}

function draft(
  over: Partial<ToolCallDraft> & { external_call_id: string; source_event_id: string },
): ToolCallDraft {
  return {
    session_id: "S",
    agent: "claude_code",
    server: "builtin",
    name: "Bash",
    turn_index: null,
    call_index: 0,
    started_at: TS,
    ended_at: null,
    status: "unknown",
    args_hash: "",
    signature_hash: "none",
    args_bytes: 0,
    result_bytes: 0,
    model: null,
    action: null,
    ...over,
  };
}

function item(
  over: Partial<QueueItem> & { source_event_id: string; session_id: string },
): QueueItem {
  return {
    agent: "claude_code",
    event: rawEvent({ source_event_id: over.source_event_id, session_id: over.session_id }),
    last_event_ts_ms: 0,
    synced: false,
    sent_batch_id: null,
    ...over,
  };
}

function seg(over: Partial<Segment> & { segment_id: string; source_event_ids: string[] }): Segment {
  return {
    session_id: "S",
    agent: "claude_code",
    started_at: TS,
    ended_at: TS,
    abstract: "Did the thing.",
    tokens: { input: 0, output: 0, cache_creation: 0, cache_read: 0, reasoning: 0 },
    tags: [],
    redaction: { secrets_found: 0, emails_redacted: 0, paths_redacted_absolute: 0 },
    ...over,
  };
}

function memStore(items: QueueItem[]): QueueStore {
  return {
    async put() {},
    async has() {
      return false;
    },
    async listUnsentBySession() {
      const m = new Map<string, QueueItem[]>();
      for (const it of items) {
        const arr = m.get(it.session_id) ?? [];
        arr.push(it);
        m.set(it.session_id, arr);
      }
      return m;
    },
    async markSent() {},
    async countUnsent() {
      return items.length;
    },
  };
}

/** One segment per pipeline.run call, covering exactly the events it
 * was handed — segment_id derived from the session so assertions can
 * tell which segment a call was attributed to. */
const echoPipeline: PipelineRunner = {
  async run(events: RawEvent[]) {
    if (events.length === 0) return [];
    return [
      seg({
        segment_id: `seg_${events[0]!.session_id}`,
        session_id: events[0]!.session_id,
        source_event_ids: events.map((e) => e.source_event_id),
      }),
    ];
  },
};

const baseOpts = {
  pipeline: echoPipeline,
  deviceId: "dev_1",
  companionVersion: "test",
  nowMs: 1_000_000, // far past every item's last_event_ts_ms=0 → quiet
} as const;

describe("attachSegmentIds", () => {
  it("fills segment_id for calls whose source event a segment covers, null otherwise", () => {
    const calls = [
      draft({ external_call_id: "c1", source_event_id: "e1" }),
      draft({ external_call_id: "c2", source_event_id: "e2" }),
      draft({ external_call_id: "c3", source_event_id: "orphan" }),
    ];
    const segments = [
      seg({ segment_id: "seg_a", source_event_ids: ["e1"] }),
      seg({ segment_id: "seg_b", source_event_ids: ["e2", "e9"] }),
    ];
    const wired = attachSegmentIds(calls, segments);
    assert.deepEqual(
      wired.map((c) => [c.external_call_id, c.segment_id]),
      [
        ["c1", "seg_a"],
        ["c2", "seg_b"],
        ["c3", null],
      ],
    );
  });

  it("returns all-null segment_ids when there are no segments", () => {
    const wired = attachSegmentIds([draft({ external_call_id: "c1", source_event_id: "e1" })], []);
    assert.deepEqual(wired[0]!.segment_id, null);
  });
});

describe("attachSegmentIdsByMap (streaming straddle attribution)", () => {
  it("keeps segment_id for a call whose event was segmented in an earlier batch", () => {
    // Streaming scan straddle: a file's events span a BATCH_MAX_EVENTS
    // flush. The early events (e1, e2) are segmented and shipped in
    // batch A (seg_a); the later events (e3, e4) in batch B (seg_b). The
    // file's tool-call drafts buffer at parse-end and ride batch B — so
    // a call on the early event e1 is being attributed in a batch whose
    // *own* segments (seg_b) don't cover it.
    const batchAsegments = [seg({ segment_id: "seg_a", source_event_ids: ["e1", "e2"] })];
    const batchBsegments = [seg({ segment_id: "seg_b", source_event_ids: ["e3", "e4"] })];
    const calls = [
      draft({ external_call_id: "c1", source_event_id: "e1" }), // straddled
      draft({ external_call_id: "c3", source_event_id: "e3" }), // in the shipping batch
      draft({ external_call_id: "c0", source_event_id: "no-segment" }), // genuine orphan
    ];

    // The pre-fix behaviour — matching only the shipping batch's
    // segments — drops the straddling call to null. This is the bug.
    const perBatch = attachSegmentIds(calls, batchBsegments);
    assert.equal(
      perBatch.find((c) => c.external_call_id === "c1")!.segment_id,
      null,
      "regression guard: per-batch attribution would lose the straddling call",
    );

    // The fix — attribute against every segment seen this run for the
    // call's session (exactly the map scan.ts's flushBatch builds from
    // runSegmentsBySession).
    const runSegmentByEvent = new Map<string, string>();
    for (const s of [...batchAsegments, ...batchBsegments]) {
      for (const id of s.source_event_ids) runSegmentByEvent.set(id, s.segment_id);
    }
    const wired = attachSegmentIdsByMap(calls, runSegmentByEvent);
    assert.deepEqual(
      wired.map((c) => [c.external_call_id, c.segment_id]),
      [
        ["c1", "seg_a"], // straddled call keeps its earlier-batch segment
        ["c3", "seg_b"],
        ["c0", null], // genuine orphan still ships null (valid wire)
      ],
    );
  });
});

describe("buildBatches tool_calls plumbing", () => {
  it("carries each item's tool_calls into the batch with segment_id filled", async () => {
    const items = [
      item({
        source_event_id: "a1",
        session_id: "A",
        tool_calls: [
          draft({ external_call_id: "c1", source_event_id: "a1", session_id: "A" }),
          draft({ external_call_id: "c2", source_event_id: "a1", session_id: "A", call_index: 1 }),
        ],
      }),
      item({ source_event_id: "a2", session_id: "A" }),
      item({ source_event_id: "b1", session_id: "B" }),
    ];
    const batches = await buildBatches({ ...baseOpts, store: memStore(items) });
    assert.equal(batches.length, 1);
    assert.equal(batches[0]!.events.length, 3);
    assert.deepEqual(
      batches[0]!.tool_calls.map((c) => [c.external_call_id, c.segment_id]),
      [
        ["c1", "seg_A"],
        ["c2", "seg_A"],
      ],
    );
  });

  it("ships empty tool_calls when no item carries any", async () => {
    const items = [item({ source_event_id: "a1", session_id: "A" })];
    const batches = await buildBatches({ ...baseOpts, store: memStore(items) });
    assert.equal(batches.length, 1);
    assert.deepEqual(batches[0]!.tool_calls, []);
  });

  it("keeps a call in the same batch as its event across a maxEvents split", async () => {
    const items = [
      item({
        source_event_id: "a1",
        session_id: "A",
        tool_calls: [draft({ external_call_id: "c1", source_event_id: "a1", session_id: "A" })],
      }),
      item({
        source_event_id: "a2",
        session_id: "A",
        tool_calls: [draft({ external_call_id: "c2", source_event_id: "a2", session_id: "A" })],
      }),
    ];
    const batches = await buildBatches({ ...baseOpts, store: memStore(items), maxEvents: 1 });
    assert.equal(batches.length, 2);
    for (const batch of batches) {
      assert.equal(batch.events.length, 1);
      assert.equal(batch.tool_calls.length, 1);
      assert.equal(batch.tool_calls[0]!.source_event_id, batch.events[0]!.source_event_id);
    }
  });

  it("splits before the per-batch tool-call cap, keeping calls with their event", async () => {
    const callsPer = 9_000; // 3 items × 9k = 27k > 20k cap → split after item 2
    const mkCalls = (eid: string) =>
      Array.from({ length: callsPer }, (_, i) =>
        draft({ external_call_id: `${eid}_c${i}`, source_event_id: eid, call_index: i }),
      );
    const items = [
      item({ source_event_id: "e1", session_id: "S", tool_calls: mkCalls("e1") }),
      item({ source_event_id: "e2", session_id: "S", tool_calls: mkCalls("e2") }),
      item({ source_event_id: "e3", session_id: "S", tool_calls: mkCalls("e3") }),
    ];
    const batches = await buildBatches({ ...baseOpts, store: memStore(items) });
    assert.equal(batches.length, 2);
    assert.equal(batches[0]!.events.length, 2);
    assert.equal(batches[0]!.tool_calls.length, 2 * callsPer);
    assert.equal(batches[1]!.events.length, 1);
    assert.equal(batches[1]!.tool_calls.length, callsPer);
    for (const batch of batches) {
      assert.ok(batch.tool_calls.length <= INGEST_BATCH_MAX_TOOL_CALLS);
      const eventIds = new Set(batch.events.map((e) => e.source_event_id));
      for (const c of batch.tool_calls) {
        assert.ok(eventIds.has(c.source_event_id), "call shipped apart from its event");
      }
      // Segment attribution ran per batch.
      assert.ok(batch.tool_calls.every((c) => c.segment_id === "seg_S"));
    }
  });

  it("holds sessions still inside the debounce window", async () => {
    const items = [
      item({ source_event_id: "a1", session_id: "A", last_event_ts_ms: baseOpts.nowMs }),
    ];
    const batches = await buildBatches({ ...baseOpts, store: memStore(items) });
    assert.deepEqual(batches, []);
  });
});
