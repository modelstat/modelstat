import assert from "node:assert/strict";
import { describe, it } from "node:test";
import type { RawEvent } from "@modelstat/core/schemas";
import {
  buildSegmentsForSession,
  type PipelineAdapters,
  temporalHints,
} from "./index.js";

describe("temporalHints", () => {
  it("buckets the engineer's LOCAL time-of-day and cadence", () => {
    // new Date(y, mIdx, d, h) is LOCAL, matching the daemon's getHours()/getDay(),
    // so this is timezone-independent (built + read in the same local tz).
    const friMorning = new Date(2026, 5, 26, 9, 0).getTime(); // Fri 2026-06-26 09:00
    assert.deepEqual(temporalHints(friMorning), [
      { root_key: "time_of_day", name: "Morning", confidence: 1 },
      { root_key: "cadence", name: "Friday", confidence: 1 },
    ]);
    const satNight = new Date(2026, 5, 27, 23, 30).getTime(); // Sat 23:30
    assert.deepEqual(temporalHints(satNight), [
      { root_key: "time_of_day", name: "Night", confidence: 1 },
      { root_key: "cadence", name: "Weekend", confidence: 1 },
    ]);
    const wedMidday = new Date(2026, 5, 24, 14, 0).getTime(); // Wed 14:00
    assert.deepEqual(temporalHints(wedMidday), [
      { root_key: "time_of_day", name: "Midday", confidence: 1 },
      { root_key: "cadence", name: "Weekday", confidence: 1 },
    ]);
  });
});

/** Minimal RawEvent for pipeline tests. Carries a content_excerpt by
 * default — summariseSlice refuses slices with zero excerpts. */
function ev(over: Partial<RawEvent> & { source_event_id: string; ts: string }): RawEvent {
  return {
    kind: "assistant_message",
    agent: "claude_code",
    provider: "anthropic",
    model: "claude-fable-5",
    session_id: "sess-1",
    turn_index: null,
    parent_event_id: null,
    cwd: null,
    git: null,
    tokens: null,
    duration_ms: null,
    tool_calls: {},
    files_touched: [],
    content_excerpt: "Investigated the failing ingest and fixed the test runner.",
    source_file: null,
    source_byte_offset: null,
    ...over,
  };
}

/** Deterministic, model-free adapters. The empty embedding makes the
 * pipeline fall back to time-gap boundary detection, so events minutes
 * apart land in one slice. */
const adapters: PipelineAdapters = {
  embed: async () => [],
  summarize: async () => "Fixed the ingest pipeline and tagged tool usage.",
  tokenize: (text: string) => Math.max(1, Math.ceil(text.length / 4)),
};

function toolCallTags(tags: Array<{ root_key: string; name: string; confidence: number }>) {
  return tags.filter((t) => t.root_key === "tool_calls");
}

describe("buildSegmentsForSession tool_calls tags", () => {
  it("aggregates per-event count maps and tags the top-8 identities by count", async () => {
    const events = [
      ev({
        source_event_id: "e1",
        ts: "2026-06-01T10:00:00.000Z",
        tool_calls: { Bash: 5, Read: 3 },
      }),
      ev({
        source_event_id: "e2",
        ts: "2026-06-01T10:01:00.000Z",
        tool_calls: {
          Bash: 5,
          "mcp:github/create_pr": 2,
          Grep: 1,
          Glob: 1,
          Edit: 1,
          Write: 1,
          WebSearch: 1,
          TodoWrite: 1,
          Task: 1,
        },
      }),
    ];
    const segments = await buildSegmentsForSession(events, adapters);
    assert.equal(segments.length, 1, "two close events make one segment");
    const tags = toolCallTags(segments[0]!.tags);
    // 10 distinct identities observed, total 22 calls; top-8 kept,
    // ties broken by name so the cut is deterministic.
    assert.deepEqual(tags, [
      { root_key: "tool_calls", name: "Bash", confidence: 0.45 }, // 10/22
      { root_key: "tool_calls", name: "Read", confidence: 0.14 }, // 3/22
      { root_key: "tool_calls", name: "mcp:github/create_pr", confidence: 0.09 }, // 2/22
      { root_key: "tool_calls", name: "Edit", confidence: 0.05 }, // 1/22 → floor
      { root_key: "tool_calls", name: "Glob", confidence: 0.05 },
      { root_key: "tool_calls", name: "Grep", confidence: 0.05 },
      { root_key: "tool_calls", name: "Task", confidence: 0.05 },
      { root_key: "tool_calls", name: "TodoWrite", confidence: 0.05 },
    ]);
    // Deterministic tags from other roots still present, total under
    // the Segment.tags wire cap.
    assert.ok(segments[0]!.tags.some((t) => t.root_key === "agents"));
    assert.ok(segments[0]!.tags.length <= 40);
  });

  it("emits no tool_calls tags when no event carries tool calls", async () => {
    const events = [
      ev({ source_event_id: "e1", ts: "2026-06-01T10:00:00.000Z" }),
      ev({ source_event_id: "e2", ts: "2026-06-01T10:01:00.000Z" }),
    ];
    const segments = await buildSegmentsForSession(events, adapters);
    assert.equal(segments.length, 1);
    assert.deepEqual(toolCallTags(segments[0]!.tags), []);
  });

  it("clamps confidence into [0.05, 1]", async () => {
    const events = [
      ev({
        source_event_id: "e1",
        ts: "2026-06-01T10:00:00.000Z",
        tool_calls: { Bash: 999, Read: 1 },
      }),
      ev({ source_event_id: "e2", ts: "2026-06-01T10:01:00.000Z" }),
    ];
    const segments = await buildSegmentsForSession(events, adapters);
    assert.equal(segments.length, 1);
    const tags = toolCallTags(segments[0]!.tags);
    // 999/1000 rounds to 1.0 and must not exceed 1; 1/1000 rounds to
    // 0.00 and must be floored to 0.05.
    assert.deepEqual(tags, [
      { root_key: "tool_calls", name: "Bash", confidence: 1 },
      { root_key: "tool_calls", name: "Read", confidence: 0.05 },
    ]);
  });

  it("ignores zero/negative counts in the aggregate map", async () => {
    const events = [
      ev({
        source_event_id: "e1",
        ts: "2026-06-01T10:00:00.000Z",
        tool_calls: { Bash: 2, Read: 0 },
      }),
      ev({ source_event_id: "e2", ts: "2026-06-01T10:01:00.000Z" }),
    ];
    const segments = await buildSegmentsForSession(events, adapters);
    const tags = toolCallTags(segments[0]!.tags);
    assert.deepEqual(tags, [{ root_key: "tool_calls", name: "Bash", confidence: 1 }]);
  });

  it("skips identities longer than the 120-char tag-name cap", async () => {
    // server (`mcp:` + ≤116) and name (≤120) are individually wire-legal,
    // but the composed identity can exceed TaxonomyHintRooted.name's
    // .max(120) — shipping it would 400 (and drop) the whole batch.
    const overlong = `mcp:${"s".repeat(116)}/${"t".repeat(120)}`;
    assert.ok(overlong.length > 120, "fixture must exceed the tag-name cap");
    const events = [
      ev({
        source_event_id: "e1",
        ts: "2026-06-01T10:00:00.000Z",
        tool_calls: { [overlong]: 9, Bash: 1 },
      }),
      ev({ source_event_id: "e2", ts: "2026-06-01T10:01:00.000Z" }),
    ];
    const segments = await buildSegmentsForSession(events, adapters);
    const tags = toolCallTags(segments[0]!.tags);
    // The overlong identity is dropped (not truncated — a truncated name
    // would mismatch the server-side leaf); Bash keeps its true share of
    // the total (1/10 → 0.1), and every shipped tag stays wire-legal.
    assert.deepEqual(tags, [{ root_key: "tool_calls", name: "Bash", confidence: 0.1 }]);
    for (const t of segments[0]!.tags) {
      assert.ok(t.name.length <= 120, `tag name over wire cap: ${t.name}`);
    }
  });
});
