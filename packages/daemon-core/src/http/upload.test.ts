/**
 * The `permanent` discriminator on a dropped upload — the bit that lets the scan
 * loop tell "this batch is poison, skip it" (400/422) from "the server blipped,
 * hold + retry" (5xx-exhausted / network). Getting this wrong either wedges the
 * whole newest-first scan behind one bad batch (drop treated as retry) or loses
 * good data on a transient blip (retry treated as drop).
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import { IngestClient } from "./index.js";

const logger = { error() {}, warn() {}, info() {}, debug() {} } as never;
const auth = { getToken: async () => "tok", onInvalidToken: async () => false };

function client(fetchImpl: typeof fetch, maxAttempts = 1) {
  return new IngestClient({ apiUrl: "http://x", auth, logger, fetchImpl, maxAttempts });
}

test("400 → PERMANENT drop (quarantine, never block the scan)", async () => {
  const res = await client(async () => new Response("bad", { status: 400 })).upload({} as never);
  assert.equal(res.kind, "drop");
  assert.equal(res.kind === "drop" && res.permanent, true);
});

test("422 → PERMANENT drop", async () => {
  const res = await client(async () => new Response("invalid", { status: 422 })).upload(
    {} as never,
  );
  assert.equal(res.kind === "drop" && res.permanent, true);
});

test("exhausted 5xx → TRANSIENT drop (hold + retry, never quarantine good data)", async () => {
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
