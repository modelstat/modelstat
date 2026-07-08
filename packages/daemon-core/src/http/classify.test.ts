/**
 * Regression tests for the upload-status retry matrix (classifyStatus).
 *
 * Core guarantee: the daemon NEVER drops a batch. Only 2xx commits and 401/403
 * recover identity + retry; EVERY other status backs off and holds, retrying
 * until the server is healthy — so an outage or misconfig is a pause, never a gap
 * in the uploaded data. This deliberately includes the 4xx codes we used to treat
 * as "permanent" (400/413/415/422): even those can be a transient Cloudflare/WAF
 * edge response or clear after a server-side clamp/parse fix, and silently losing
 * usage data is worse than a visible, self-draining retry.
 *
 * History pinned here: 426 (version gate — daemon self-updates then succeeds) and
 * 404/405 (endpoint unroutable: undeployed / mis-proxied / renamed mid-rollout;
 * observed live 2026-07-08 when a Caddy exact-path matcher 405'd every cloud-mode
 * POST /v1/ingest/raw and the old default dropped the data) must both back off.
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import { classifyStatus } from "./index.js";

test("2xx commits", () => {
  assert.equal(classifyStatus(200, 0).type, "commit");
  assert.equal(classifyStatus(204, 0).type, "commit");
});

test("auth errors reauth (recover identity + retry — not a drop)", () => {
  assert.equal(classifyStatus(401, 0).type, "reauth");
  assert.equal(classifyStatus(403, 0).type, "reauth");
});

test("every other status backs off — the daemon NEVER drops a batch", () => {
  // Includes the former "permanent" payload codes (400/413/415/422), the
  // version-gate (426), unroutable-endpoint (404/405), timeouts/rate-limits
  // (408/429), 5xx, and anything unrecognized (402/409/410/451).
  const holds = [
    400, 402, 404, 405, 408, 409, 410, 413, 415, 422, 426, 429, 500, 502, 503, 504,
  ];
  for (const s of holds) {
    assert.equal(classifyStatus(s, 0).type, "backoff", `status ${s} must back off (never drop)`);
  }
});

test("no status in 200–599 yields a drop decision", () => {
  // The strong guarantee: there is no code path that discards a batch.
  for (let s = 200; s <= 599; s++) {
    assert.notEqual(classifyStatus(s, 0).type, "drop", `status ${s} must not drop`);
  }
});
