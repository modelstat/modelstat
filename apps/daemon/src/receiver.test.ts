/**
 * Receiver tests — the loopback ingest server's HTTP contract:
 * acceptance, idempotent dedupe, loopback-trust (no auth required), and
 * input validation. The drain path (buildBatches → summariser → upload) is
 * exercised elsewhere; here we pin the surface the SDKs actually POST to.
 *
 * Runs against an ephemeral port (`port: 0`) and a throwaway MODELSTAT_HOME
 * so it never collides with a real daemon on 4319 or a real queue file.
 */
import assert from "node:assert/strict";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";
import { type LocalIngestReceiver, localQueueDepth, startLocalIngestReceiver } from "./receiver.js";

let recv: LocalIngestReceiver | null = null;
let base = "";

before(async () => {
  // Point the daemon home at a throwaway dir BEFORE the receiver lazily
  // opens its FileQueueStore on the first request.
  process.env.MODELSTAT_HOME = mkdtempSync(join(tmpdir(), "modelstat-recv-"));
  recv = await startLocalIngestReceiver({ port: 0 });
  assert.ok(recv, "receiver should bind on an ephemeral port");
  base = `http://127.0.0.1:${recv.port}`;
});

after(async () => {
  await recv?.close();
});

function event(id: string, session = "sess_a"): Record<string, unknown> {
  return {
    source_event_id: id,
    ts: new Date(0).toISOString(),
    kind: "assistant_message",
    agent: "raw_sdk_openai",
    provider: "openai",
    session_id: session,
    tokens: { input: 10, output: 5, cache_creation: 0, cache_read: 0, reasoning: 0 },
    content_excerpt: "redacted excerpt",
  };
}

function post(body: string, init?: RequestInit): Promise<Response> {
  return fetch(`${base}/v1/ingest`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body,
    ...init,
  });
}

test("a valid batch is accepted and enqueued", async () => {
  const depth0 = await localQueueDepth();
  const res = await post(
    JSON.stringify({
      batch_id: "batch_1",
      device_id: "dev_sdk",
      daemon_version: "node-sdk/0",
      events: [event("evt_1"), event("evt_2")],
    }),
    { headers: { "content-type": "application/json", authorization: "Bearer ignored" } },
  );
  assert.equal(res.status, 200);
  assert.deepEqual(await res.json(), { accepted: 2, queued: true });
  assert.equal(await localQueueDepth(), depth0 + 2);
});

test("re-POST of the same events is idempotent (dedupe by source_event_id)", async () => {
  const depth0 = await localQueueDepth();
  const res = await post(
    JSON.stringify({
      batch_id: "batch_2",
      device_id: "dev_sdk",
      daemon_version: "node-sdk/0",
      events: [event("evt_1"), event("evt_2")],
    }),
  );
  assert.equal(res.status, 200);
  assert.equal(await localQueueDepth(), depth0, "duplicate events must not grow the queue");
});

test("loopback trust: a request with no Authorization header is accepted", async () => {
  const res = await post(
    JSON.stringify({
      batch_id: "batch_3",
      device_id: "dev_sdk",
      daemon_version: "node-sdk/0",
      events: [event("evt_keyless")],
    }),
  );
  assert.equal(res.status, 200);
});

test("invalid json → 400", async () => {
  const res = await post("{ not json");
  assert.equal(res.status, 400);
});

test("empty events array → 400", async () => {
  const res = await post(JSON.stringify({ events: [] }));
  assert.equal(res.status, 400);
});

test("an event missing a required field → 400", async () => {
  const res = await post(JSON.stringify({ events: [{ source_event_id: "x" }] }));
  assert.equal(res.status, 400);
});

test("GET /healthz → 200, unknown path → 404", async () => {
  assert.equal((await fetch(`${base}/healthz`)).status, 200);
  assert.equal((await fetch(`${base}/nope`)).status, 404);
});
