/**
 * Regression tests for the upload-status retry matrix (classifyStatus).
 *
 * Pins the version-gate fix: an HTTP 426 ("daemon too old") is TRANSIENT — the
 * daemon auto-updates via its heartbeat — so it must BACK OFF (hold + retry the
 * batch), never `drop` it. Dropping logged an error and re-spun the same data
 * every scan cycle until the update landed.
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

test("permanent client errors drop", () => {
  assert.equal(classifyStatus(400, 0).type, "drop");
  assert.equal(classifyStatus(422, 0).type, "drop");
});

test("auth errors reauth", () => {
  assert.equal(classifyStatus(401, 0).type, "reauth");
  assert.equal(classifyStatus(403, 0).type, "reauth");
});
