/**
 * Regression tests for streaming parse (ParserContext.onEvents).
 *
 * Memory contract pinned here: with a sink attached, a parser must
 * (a) deliver events in chunks of at most PARSER_EVENT_CHUNK,
 * (b) leave ParseResult.events EMPTY — never accumulate the file, and
 * (c) produce exactly the same events, in the same order, as a
 *     non-streaming parse.
 * This is what keeps a full-corpus reprocess (cursor wipe) at bounded
 * memory in the agent's scan loop — see apps/daemon/src/scan.ts.
 */

import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import type { RawEvent } from "@modelstat/core";
import { parseClaudeCodeJsonl } from "./claude-code/index.js";
import { parseCodexRollout } from "./codex/index.js";
import { parsePiSession } from "./pi/index.js";
import { PARSER_EVENT_CHUNK } from "./types.js";

const SESSION = "11111111-2222-3333-4444-555555555555";

function claudeCodeLines(turns: number): string {
  const lines: string[] = [];
  for (let i = 0; i < turns; i++) {
    const ts = new Date(Date.UTC(2026, 0, 1, 0, 0, i)).toISOString();
    if (i % 2 === 0) {
      lines.push(
        JSON.stringify({
          type: "user",
          uuid: `u-${i}`,
          timestamp: ts,
          sessionId: SESSION,
          cwd: "/Users/someone/proj",
          message: { role: "user", content: `please do thing number ${i}` },
        }),
      );
    } else {
      lines.push(
        JSON.stringify({
          type: "assistant",
          uuid: `a-${i}`,
          timestamp: ts,
          sessionId: SESSION,
          cwd: "/Users/someone/proj",
          message: {
            role: "assistant",
            model: "claude-test-1",
            content: [{ type: "text", text: `did thing number ${i}` }],
            usage: { input_tokens: 10, output_tokens: 5 },
          },
        }),
      );
    }
  }
  return `${lines.join("\n")}\n`;
}

function piLines(turns: number): string {
  const lines: string[] = [
    JSON.stringify({ type: "session", id: SESSION, cwd: "/Users/someone/proj" }),
    JSON.stringify({ type: "model_change", provider: "anthropic", modelId: "claude-test-1" }),
  ];
  for (let i = 0; i < turns; i++) {
    const ts = new Date(Date.UTC(2026, 0, 1, 0, 0, i)).toISOString();
    const message =
      i % 2 === 0
        ? { role: "user", content: [{ type: "text", text: `please do thing number ${i}` }] }
        : {
            role: "assistant",
            provider: "anthropic",
            model: "claude-test-1",
            content: [{ type: "text", text: `did thing number ${i}` }],
            usage: { input: 10, output: 5 },
          };
    lines.push(JSON.stringify({ type: "message", timestamp: ts, message }));
  }
  return `${lines.join("\n")}\n`;
}

function codexLines(turns: number): string {
  const lines: string[] = [JSON.stringify({ type: "session_meta", id: SESSION })];
  for (let i = 0; i < turns; i++) {
    const ts = new Date(Date.UTC(2026, 0, 1, 0, 0, i)).toISOString();
    lines.push(
      JSON.stringify({
        type: "event_msg",
        timestamp: ts,
        payload: { type: "token_count", input_tokens: 10 + i, output_tokens: 5 },
      }),
    );
  }
  return `${lines.join("\n")}\n`;
}

async function withTempFile(
  name: string,
  content: string,
  fn: (path: string) => Promise<void>,
): Promise<void> {
  const dir = await mkdtemp(join(tmpdir(), "modelstat-parsers-"));
  try {
    const p = join(dir, name);
    await writeFile(p, content, "utf8");
    await fn(p);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

for (const [label, fixture, parse, expectedEvents] of [
  // 600 JSONL turns → 600 events (every line emits), 3 chunks of ≤256.
  ["claude-code", claudeCodeLines(600), parseClaudeCodeJsonl, 600],
  // 600 token_count lines → 600 events; session_meta emits nothing.
  ["codex", codexLines(600), parseCodexRollout, 600],
  // 600 message lines → 600 events; session/model_change emit nothing.
  ["pi", piLines(600), parsePiSession, 600],
] as const) {
  test(`${label}: streaming parse is chunk-bounded and equivalent to buffered parse`, async () => {
    await withTempFile(`${label}.jsonl`, fixture, async (path) => {
      const buffered = await parse({ deviceId: "dev-test", sourceFile: path });
      assert.equal(buffered.events.length, expectedEvents);

      const chunks: RawEvent[][] = [];
      const streamed = await parse({
        deviceId: "dev-test",
        sourceFile: path,
        onEvents: (events) => {
          chunks.push(events);
        },
      });

      assert.equal(
        streamed.events.length,
        0,
        "streaming parse must not also accumulate events in the result",
      );
      assert.equal(streamed.stats.emittedEvents, expectedEvents);
      assert.ok(chunks.length > 1, "fixture should span multiple chunks");
      for (const c of chunks) {
        assert.ok(c.length >= 1 && c.length <= PARSER_EVENT_CHUNK);
      }
      assert.deepEqual(
        chunks.flat(),
        buffered.events,
        "streamed events must match buffered parse exactly, in order",
      );
    });
  });
}

test("claude-code: slow sink applies backpressure (chunks delivered sequentially)", async () => {
  await withTempFile("backpressure.jsonl", claudeCodeLines(600), async (path) => {
    let inSink = false;
    let maxObservedOverlap = 0;
    await parseClaudeCodeJsonl({
      deviceId: "dev-test",
      sourceFile: path,
      onEvents: async () => {
        if (inSink) maxObservedOverlap += 1;
        inSink = true;
        await new Promise((r) => setTimeout(r, 5));
        inSink = false;
      },
    });
    assert.equal(maxObservedOverlap, 0, "parser must await the sink before reading on");
  });
});
