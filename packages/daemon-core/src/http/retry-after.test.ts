/**
 * Tests for Retry-After honoring + jitter (the client half of the ingest edge's
 * 429/503 load-shedding). The server sends Retry-After on backpressure; the
 * client must wait at least that long, plus jitter so a fleet doesn't retry in
 * lockstep and re-create the overload.
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import { jitter, retryAfterMs } from "./index.js";

function resWith(retryAfter?: string): Response {
  const headers = new Headers();
  if (retryAfter !== undefined) headers.set("retry-after", retryAfter);
  return new Response(null, { status: 429, headers });
}

test("retryAfterMs parses delta-seconds into ms", () => {
  assert.equal(retryAfterMs(resWith("5")), 5000);
  assert.equal(retryAfterMs(resWith("0")), 0);
  assert.equal(retryAfterMs(resWith(" 2 ")), 2000);
});

test("retryAfterMs is 0 when the header is absent or unparseable", () => {
  assert.equal(retryAfterMs(resWith(undefined)), 0);
  assert.equal(retryAfterMs(resWith("soon")), 0);
  assert.equal(retryAfterMs(resWith("-1")), 0);
});

test("jitter keeps ms as a floor and adds at most 25%", () => {
  for (let i = 0; i < 500; i++) {
    const j = jitter(1000);
    assert.ok(j >= 1000, `must not undercut the floor, got ${j}`);
    assert.ok(j < 1250, `must add at most 25%, got ${j}`);
  }
  assert.equal(jitter(0), 0);
  assert.equal(jitter(-5), 0);
});
