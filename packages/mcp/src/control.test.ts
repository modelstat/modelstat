/**
 * Control-kick tests — the eager flow's call to the local daemon's loopback
 * control endpoint: success, server error, and connection-refused (no daemon)
 * are mapped to the right outcome so the tool call can decide whether to
 * proceed. Runs against an ephemeral HTTP server (or a guaranteed-closed port).
 */
import assert from "node:assert/strict";
import { createServer, type Server } from "node:http";
import { after, before, test } from "node:test";
import { kickDaemonScan } from "./control.js";

let srv: Server | null = null;
let port = 0;
let lastBody: unknown = null;
let nextStatus = 200;

before(async () => {
  srv = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c) => chunks.push(c as Buffer));
    req.on("end", () => {
      lastBody = JSON.parse(Buffer.concat(chunks).toString("utf8") || "{}");
      res.writeHead(nextStatus, { "content-type": "application/json" });
      res.end(JSON.stringify({ ok: nextStatus < 300, scanned: true }));
    });
  });
  await new Promise<void>((r) => srv?.listen(0, "127.0.0.1", r));
  const addr = srv?.address();
  port = typeof addr === "object" && addr ? addr.port : 0;
  process.env.MODELSTAT_LOCAL_INGEST_PORT = String(port);
});

after(async () => {
  await new Promise<void>((r) => srv?.close(() => r()));
});

test("empty session ids → no_daemon (nothing to scan)", async () => {
  const r = await kickDaemonScan([]);
  assert.deepEqual(r, { kind: "no_daemon" });
});

test("a 200 from the daemon → scanned, with session_ids + wait forwarded", async () => {
  nextStatus = 200;
  const r = await kickDaemonScan(["s1", "s2"], { wait: true });
  assert.deepEqual(r, { kind: "scanned" });
  assert.deepEqual(lastBody, { session_ids: ["s1", "s2"], wait: true });
});

test("a 4xx/5xx from the daemon → error (caller proceeds anyway)", async () => {
  nextStatus = 500;
  const r = await kickDaemonScan(["s1"]);
  assert.equal(r.kind, "error");
});

test("connection refused on a known-closed port → no_daemon", async () => {
  // Bind then immediately close a server to get a port guaranteed closed.
  const tmp = createServer();
  const closedPort: number = await new Promise((resolve) => {
    tmp.listen(0, "127.0.0.1", () => {
      const a = tmp.address();
      const p = typeof a === "object" && a ? a.port : 0;
      tmp.close(() => resolve(p));
    });
  });
  process.env.MODELSTAT_LOCAL_INGEST_PORT = String(closedPort);
  const r = await kickDaemonScan(["s1"], { timeoutMs: 2000 });
  assert.deepEqual(r, { kind: "no_daemon" });
  // restore the live port for any later test ordering
  process.env.MODELSTAT_LOCAL_INGEST_PORT = String(port);
});
