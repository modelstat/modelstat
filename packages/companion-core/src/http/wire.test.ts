/**
 * Regression tests for the ingest wire format.
 *
 * Pins the 2026-06-12 shipping wedge: excerpt/abstract truncation via
 * `.slice(0, N)` can cut an emoji in half, leaving a lone UTF-16
 * surrogate; JSON.stringify emits `"\ud83d"` which the ingest server's
 * strict JSON decoder rejects with "unexpected end of
 * hex escape" → HTTP 400 → the whole batch drops, and the same poison
 * event re-parses on every scan, wedging that file forever. Every
 * upload body must therefore contain only well-formed strings.
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import type { IngestBatch } from "@modelstat/core/schemas";
import { IngestClient, wellFormedStringify } from "./index.js";

const LONE_LEADING = "truncated emoji: \u{1F600}".slice(0, 18); // ends mid-surrogate-pair

function assertWellFormedJson(body: string): void {
  // Body must round-trip through a parse with every string well-formed
  // — i.e. no lone surrogates survived to the wire.
  const parsed = JSON.parse(body) as unknown;
  const walk = (v: unknown): void => {
    if (typeof v === "string") {
      assert.equal(v, v.toWellFormed(), `string not well-formed: ${JSON.stringify(v)}`);
    } else if (Array.isArray(v)) {
      for (const x of v) walk(x);
    } else if (v && typeof v === "object") {
      for (const x of Object.values(v)) walk(x);
    }
  };
  walk(parsed);
}

test("fixture sanity: the truncated excerpt really is malformed", () => {
  assert.notEqual(LONE_LEADING, LONE_LEADING.toWellFormed());
});

test("wellFormedStringify scrubs lone surrogates anywhere in the value tree", () => {
  const body = wellFormedStringify({
    abstract: LONE_LEADING,
    nested: { excerpts: [LONE_LEADING, "fine"], n: 3 },
  });
  assertWellFormedJson(body);
  // Non-string values and intact emoji pass through untouched.
  const intact = wellFormedStringify({ s: "full emoji \u{1F600}", n: 1 });
  assert.deepEqual(JSON.parse(intact), { s: "full emoji \u{1F600}", n: 1 });
});

test("IngestClient.upload sends a body with no lone surrogates", async () => {
  let sentBody: string | null = null;
  const client = new IngestClient({
    apiUrl: "http://ingest.test",
    auth: { getToken: async () => "tok", onInvalidToken: async () => false },
    logger: { debug() {}, info() {}, warn() {}, error() {} } as never,
    fetchImpl: (async (_url: unknown, init?: { body?: unknown }) => {
      sentBody = String(init?.body);
      return new Response(
        JSON.stringify({
          accepted: 1,
          new_sessions: 0,
          updated_sessions: 0,
          batch_id: "b",
          raw_s3_key: null,
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }) as typeof fetch,
  });

  const batch = {
    batch_id: "b",
    device_id: "d",
    companion_version: "t",
    events: [],
    segments: [{ abstract: LONE_LEADING } as never],
  } as unknown as IngestBatch;

  const res = await client.upload(batch);
  assert.equal(res.kind, "commit");
  assert.ok(sentBody, "fetch should have been called with a body");
  assertWellFormedJson(sentBody as unknown as string);
});
