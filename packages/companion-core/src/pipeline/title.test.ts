import assert from "node:assert/strict";
import { describe, it } from "node:test";
import type { Segment } from "@modelstat/core/schemas";
import {
  buildSessionTitles,
  buildTitleUserPrompt,
  fallbackTitle,
  sampleAbstracts,
  sanitiseTitle,
  stripCognitionSuffix,
  TITLE_MAX_CHARS,
  TITLER_MAX_ABSTRACTS,
} from "./title.js";

function seg(over: Partial<Segment> & { session_id: string; abstract: string }): Segment {
  return {
    segment_id: "s".repeat(64),
    agent: "claude_code",
    started_at: "2026-06-01T10:00:00.000Z",
    ended_at: "2026-06-01T10:10:00.000Z",
    tokens: { input: 1, output: 1, cache_creation: 0, cache_read: 0, reasoning: 0 },
    tags: [],
    redaction: { secrets_found: 0, emails_redacted: 0, paths_redacted_absolute: 0 },
    source_event_ids: ["e1"],
    ...over,
  };
}

describe("sanitiseTitle", () => {
  it("strips quotes, fences, trailing period and collapses whitespace", () => {
    assert.equal(
      sanitiseTitle('  "Block sync race fixes ·  hot reload." '),
      "Block sync race fixes · hot reload",
    );
    assert.equal(sanitiseTitle("```\nPaddle integration\n```"), "Paddle integration");
  });

  it("keeps only the first line of a runaway answer", () => {
    assert.equal(
      sanitiseTitle("Taxonomy filtering\nThe session also covered other things."),
      "Taxonomy filtering",
    );
  });

  it("caps to TITLE_MAX_CHARS", () => {
    const long = "x".repeat(500);
    assert.ok(sanitiseTitle(long).length <= TITLE_MAX_CHARS);
  });

  it("returns empty string on null/empty/whitespace", () => {
    assert.equal(sanitiseTitle(null), "");
    assert.equal(sanitiseTitle("   "), "");
    assert.equal(sanitiseTitle('""'), "");
  });
});

describe("stripCognitionSuffix", () => {
  it("removes Mood and Mind suffixes anywhere at the tail", () => {
    assert.equal(
      stripCognitionSuffix("Fixed the race. [Mood: frustrated, curious] [Mind: debugging]"),
      "Fixed the race.",
    );
    assert.equal(stripCognitionSuffix("No suffix here."), "No suffix here.");
  });
});

describe("fallbackTitle", () => {
  it("uses the first non-empty abstract's first sentence", () => {
    assert.equal(
      fallbackTitle(["", "Implemented Paddle checkout webhooks. Then refactored billing."]),
      "Implemented Paddle checkout webhooks",
    );
  });

  it("returns empty when no abstracts are usable", () => {
    assert.equal(fallbackTitle(["", "  "]), "");
  });
});

describe("sampleAbstracts", () => {
  it("passes through short lists and samples long ones keeping first + last", () => {
    const short = ["a", "b", "c"];
    assert.deepEqual(sampleAbstracts(short), short);
    const long = Array.from({ length: 40 }, (_, i) => `a${i}`);
    const picked = sampleAbstracts(long);
    assert.equal(picked.length, TITLER_MAX_ABSTRACTS);
    assert.equal(picked[0], "a0");
    assert.equal(picked[picked.length - 1], "a39");
  });
});

describe("buildTitleUserPrompt", () => {
  it("numbers parts chronologically and includes facts", () => {
    const p = buildTitleUserPrompt({
      abstracts: ["Did one thing.", "Did another."],
      facts: "repo org/repo; 2 parts on claude_code",
    });
    assert.match(p, /Session context: repo org\/repo; 2 parts on claude_code\./);
    assert.match(p, /\[part 1\] Did one thing\./);
    assert.match(p, /\[part 2\] Did another\./);
    assert.match(p, /Write the title\./);
  });
});

describe("buildSessionTitles", () => {
  it("titles each session via the entitler, sanitised", async () => {
    const segments = [
      seg({ session_id: "A", abstract: "Fixed block sync races. [Mood: focused]" }),
      seg({
        session_id: "A",
        abstract: "Added hot reload.",
        started_at: "2026-06-01T11:00:00.000Z",
      }),
      seg({ session_id: "B", abstract: "Wrote dashboard charts." }),
    ];
    const calls: string[][] = [];
    const titles = await buildSessionTitles(segments, async (input) => {
      calls.push(input.abstracts);
      return input.abstracts.length > 1 ? '"Block sync · hot reload."' : "Dashboard charts";
    });
    assert.deepEqual(titles, {
      A: "Block sync · hot reload",
      B: "Dashboard charts",
    });
    // Cognition suffix must not reach the entitler.
    assert.deepEqual(calls[0], ["Fixed block sync races.", "Added hot reload."]);
  });

  it("falls back deterministically when the entitler fails or is absent", async () => {
    const segments = [
      seg({ session_id: "A", abstract: "Implemented Paddle checkout. More detail after." }),
    ];
    const failing = await buildSessionTitles(segments, async () => {
      throw new Error("model unavailable");
    });
    assert.deepEqual(failing, { A: "Implemented Paddle checkout" });
    const absent = await buildSessionTitles(segments);
    assert.deepEqual(absent, { A: "Implemented Paddle checkout" });
    const noisy = await buildSessionTitles(segments, async () => "   ");
    assert.deepEqual(noisy, { A: "Implemented Paddle checkout" });
  });

  it("omits sessions with no usable abstracts", async () => {
    const titles = await buildSessionTitles(
      [seg({ session_id: "A", abstract: "  " })],
      async () => "Should never be called",
    );
    assert.deepEqual(titles, {});
  });
});
