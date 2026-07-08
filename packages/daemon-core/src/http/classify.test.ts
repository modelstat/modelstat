/**
 * Regression tests for the upload-status retry matrix (classifyStatus).
 *
 * Pins the version-gate fix: an HTTP 426 ("daemon too old") is TRANSIENT — the
 * daemon auto-updates via its heartbeat — so it must BACK OFF (hold + retry the
 * batch), never `drop` it. Dropping logged an error and re-spun the same data
 * every scan cycle until the update landed.
 *
 * Also pins the 404/405 fix: an unroutable endpoint (route not deployed yet, a
 * reverse proxy that doesn't forward the path, a route renamed mid-rollout) is
 * ENVIRONMENTAL, not a bad batch — it must BACK OFF (hold + retry until the
 * server is healthy), never `drop`. Only a payload-level reject (400/413/415/422)
 * is permanent. (Observed live 2026-07-08: a Caddy exact-path matcher 405'd every
 * cloud-mode POST /v1/ingest/raw and the old default silently dropped the data.)
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import { classifyStatus } from "./index.js";

test("2xx commits", () => {
  assert.equal(classifyStatus(200, 0).type, "commit");
  assert.equal(classifyStatus(204, 0).type, "commit");
});

test("426 (version gate) backs off — never drops", () => {
  assert.equal(classifyStatus(426, 0).type, "backoff");
});

test("transient statuses back off", () => {
  for (const s of [408, 429, 500, 502, 503]) {
    assert.equal(classifyStatus(s, 0).type, "backoff", `status ${s} should back off`);
  }
});

test("404/405 (unroutable endpoint) back off — never drop", () => {
  // The endpoint isn't reachable RIGHT NOW (undeployed / mis-proxied / renamed).
  // Environmental, not a bad batch: hold + retry so no usage data is lost.
  assert.equal(classifyStatus(404, 0).type, "backoff");
  assert.equal(classifyStatus(405, 0).type, "backoff");
});

test("unknown non-payload statuses default to back off, not drop", () => {
  // Anything we don't explicitly recognize is held, never dropped — losing data
  // is worse than a stalled retry we can see in the logs.
  for (const s of [402, 409, 410, 451]) {
    assert.equal(classifyStatus(s, 0).type, "backoff", `status ${s} should back off`);
  }
});

test("payload-level rejects drop (permanent — the batch itself is unacceptable)", () => {
  for (const s of [400, 413, 415, 422]) {
    assert.equal(classifyStatus(s, 0).type, "drop", `status ${s} should drop`);
  }
});

test("auth errors reauth", () => {
  assert.equal(classifyStatus(401, 0).type, "reauth");
  assert.equal(classifyStatus(403, 0).type, "reauth");
});
