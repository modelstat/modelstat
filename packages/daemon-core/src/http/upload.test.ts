/**
 * upload() has exactly two outcomes: `commit` (2xx) or `drop` (everything else).
 * A `drop` is ALWAYS a HOLD — the caller retries the same batch until it succeeds,
 * so a batch is never discarded. Even a 400/422 — which we used to quarantine as
 * an un-acceptable payload — is held, because it can be a transient edge/WAF
 * response or clear after a server-side fix, and the client can no longer emit a
 * genuinely-un-acceptable payload (well-formed + byte-clamped at the wire). There
 * is no "permanent" outcome to model. These tests pin that: no response commits
 * except a 2xx, and everything else is a held drop.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import { IngestClient } from "./index.js";

const logger = { error() {}, warn() {}, info() {}, debug() {} } as never;
const auth = { getToken: async () => "tok", onInvalidToken: async () => false };

function client(fetchImpl: typeof fetch, maxAttempts = 1) {
  return new IngestClient({ apiUrl: "http://x", auth, logger, fetchImpl, maxAttempts });
}

test("400 is HELD (drop), never quarantined", async () => {
  const res = await client(async () => new Response("bad", { status: 400 })).upload({} as never);
  assert.equal(res.kind, "drop");
});

test("422 is HELD (drop), never quarantined", async () => {
  const res = await client(async () => new Response("invalid", { status: 422 })).upload(
    {} as never,
  );
  assert.equal(res.kind, "drop");
});

test("exhausted 5xx → held (drop — hold + retry, never quarantine good data)", async () => {
  const res = await client(async () => new Response("down", { status: 503 })).upload({} as never);
  assert.equal(res.kind, "drop");
});

test("2xx → commit", async () => {
  const res = await client(
    async () =>
      new Response(JSON.stringify({ accepted: 1 }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
  ).upload({} as never);
  assert.equal(res.kind, "commit");
});

test("upload() targets /v1/ingest by default and /v1/ingest/raw when raw", async () => {
  const seen: string[] = [];
  const ok = async (url: string | URL | Request) => {
    seen.push(String(url));
    return new Response(JSON.stringify({ accepted: 0 }), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  };
  await client(ok as never).upload({} as never);
  await client(ok as never).upload({} as never, { raw: true });
  assert.deepEqual(seen, ["http://x/v1/ingest", "http://x/v1/ingest/raw"]);
});
