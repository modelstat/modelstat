import { strict as assert } from "node:assert";
import { test } from "node:test";
import { computeCostUsd, loadPrices, priceFor } from "./index.js";

// These tests assert the cost FORMULA against whatever rates the price
// YAML currently carries (read back via priceFor), not hardcoded dollar
// amounts. The numbers under /prices are deliberate placeholders today
// (see prices/README.md), so pinning literal costs here would re-break
// the suite the moment real rates land. Testing the formula keeps them
// correct across that swap.

const AT = new Date("2026-04-01");

test("computeCostUsd applies the loaded input + output rates", async () => {
  await loadPrices();
  const p = priceFor("anthropic", "claude-opus-4-6", AT);
  assert.ok(p, "claude-opus-4-6 should have a price point");
  const cost = computeCostUsd(
    "anthropic",
    "claude-opus-4-6",
    { input: 100_000, output: 10_000 },
    AT,
  );
  const expected = (p.input * 100_000 + p.output * 10_000) / 1_000_000;
  assert.ok(Math.abs(cost - expected) < 1e-9, `got ${cost}, expected ${expected}`);
});

test("[1m] context-window suffix resolves to the base model's rate", async () => {
  await loadPrices();
  const base = priceFor("anthropic", "claude-opus-4-6", AT);
  const aliased = priceFor("anthropic", "claude-opus-4-6[1m]", AT);
  assert.ok(base && aliased, "both the bare id and the [1m] form should resolve");
  // normaliseModelId strips the [1m] suffix, so the long-context form maps
  // to the same price point as the base model (no separate premium today).
  assert.equal(aliased.input, base.input);
  const cost = computeCostUsd("anthropic", "claude-opus-4-6[1m]", { input: 1_000_000 }, AT);
  assert.ok(
    Math.abs(cost - base.input) < 1e-9,
    `1M input tokens should cost base.input (${base.input}) USD, got ${cost}`,
  );
});

test("unknown model → 0 and no price point", async () => {
  await loadPrices();
  assert.equal(computeCostUsd("anthropic", "does-not-exist", { input: 10 }), 0);
  assert.equal(priceFor("anthropic", "does-not-exist"), null);
});

test("cache-read is cheaper than fresh input", async () => {
  await loadPrices();
  const cache = computeCostUsd("anthropic", "claude-opus-4-6", { cache_read: 1_000_000 });
  const input = computeCostUsd("anthropic", "claude-opus-4-6", { input: 1_000_000 });
  assert.ok(cache < input, `cache ${cache} should be < input ${input}`);
});
