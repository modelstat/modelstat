/**
 * Insights-cache tests — the on-disk contract the statusline depends on:
 * cache files land under ~/.modelstat/sessions/<id>.json, are atomic, and
 * round-trip the payload (with a daemon-stamped cached_at). The network fetch
 * + poll loop are exercised against a real server elsewhere; here we pin the
 * local cache behaviour with a throwaway MODELSTAT_HOME.
 */
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { before, test } from "node:test";

before(() => {
  process.env.MODELSTAT_HOME = mkdtempSync(join(tmpdir(), "modelstat-insights-"));
});

test("sessionInsightsPath lives under the daemon home's sessions dir", async () => {
  const { sessionInsightsPath } = await import("./insights.js");
  const p = sessionInsightsPath("abc-123");
  assert.ok(p.endsWith(join("sessions", "abc-123.json")), p);
  assert.ok(p.startsWith(process.env.MODELSTAT_HOME as string), p);
});

test("cacheSessionInsights writes round-trippable JSON with a cached_at stamp", async () => {
  const { cacheSessionInsights, sessionInsightsPath } = await import("./insights.js");
  const insights = {
    status: "ready" as const,
    tokens: { input: 100, output: 50, total: 150 },
    cost_usd: "0.42",
    taxonomy_nodes: [{ id: "n1", name: "debugging" }],
  };
  await cacheSessionInsights("sess-1", insights);
  const onDisk = JSON.parse(readFileSync(sessionInsightsPath("sess-1"), "utf8"));
  assert.equal(onDisk.status, "ready");
  assert.equal(onDisk.cost_usd, "0.42");
  assert.equal(onDisk.tokens.total, 150);
  assert.equal(onDisk.taxonomy_nodes[0].name, "debugging");
  assert.ok(typeof onDisk.cached_at === "string", "cached_at stamped");
});
