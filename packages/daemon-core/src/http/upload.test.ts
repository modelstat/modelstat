/**
 * upload() never returns a PERMANENT drop: the daemon holds + retries every
 * failure until it succeeds, so a batch is never discarded. Even a 400/422 —
 * which we used to quarantine as an un-acceptable payload — is held (permanent:
 * false) and retried, because it can be a transient edge/WAF response or clear
 * after a server-side fix. Losing good data is worse than a visible, self-draining
 * retry. These tests pin that: no response yields `permanent: true`.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import { IngestClient } from "./index.js";

const logger = { error() {}, warn() {}, info() {}, debug() {} } as never;
const auth = { getToken: async () => "tok", onInvalidToken: async () => false };

function client(fetchImpl: typeof fetch, maxAttempts = 1) {
  return new IngestClient({ apiUrl: "http://x", auth, logger, fetchImpl, maxAttempts });
}

test("400 is HELD (permanent:false), never quarantined", async () => {
  const res = await client(async () => new Response("bad", { status: 400 })).upload({} as never);
  assert.equal(res.kind, "drop");
  assert.equal(res.kind === "drop" && res.permanent, false);
});

test("422 is HELD (permanent:false), never quarantined", async () => {
  const res = await client(async () => new Response("invalid", { status: 422 })).upload(
    {} as never,
  );
  assert.equal(res.kind === "drop" && res.permanent, false);
});

test("exhausted 5xx → held (permanent:false — hold + retry, never quarantine good data)", async () => {
  const res = await client(async () => new Response("down", { status: 503 })).upload({} as never);
  assert.equal(res.kind, "drop");
  assert.equal(res.kind === "drop" && res.permanent, false);
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
