import assert from "node:assert/strict";
import test from "node:test";
import {
  batchId,
  contentHash,
  sourceEventId,
  totalTokens,
  zeroTokens,
} from "../index.js";

test("sourceEventId is deterministic and prefixed", () => {
  const a = sourceEventId("dev_1", "sess::100::1");
  const b = sourceEventId("dev_1", "sess::100::1");
  assert.equal(a, b);
  assert.notEqual(a, sourceEventId("dev_1", "sess::100::2"));
  assert.ok(a.startsWith("evt_"));
  assert.equal(a.length, "evt_".length + 32);
});

test("batchId is order-independent and prefixed", () => {
  const ids1 = ["evt_a", "evt_b"];
  const ids2 = ["evt_b", "evt_a"];
  assert.equal(batchId(ids1), batchId(ids2));
  assert.ok(batchId(ids1).startsWith("batch_"));
});

test("contentHash golden vector: length, determinism, separator-sensitivity", () => {
  // 32-hex truncation.
  assert.equal(contentHash(["a", "b"]).length, 32);
  // Deterministic.
  assert.equal(contentHash(["a", "b"]), contentHash(["a", "b"]));
  // The unit-separator join makes ["a","b"] differ from ["ab",""].
  assert.notEqual(contentHash(["a", "b"]), contentHash(["ab", ""]));
});

test("contentHash matches the blake3 reference value for a\\x1fb", () => {
  // Pins our derivation to a concrete blake3 output (see /tmp verification):
  // bytesToHex(blake3("a\x1fb")).slice(0,32).
  assert.equal(contentHash(["a", "b"]), "de57a552cdb05b71bc0ae3db23c33cbf");
});

test("token helpers: zeros and saturating-style total", () => {
  const z = zeroTokens();
  assert.deepEqual(z, {
    input: 0,
    output: 0,
    cache_creation: 0,
    cache_read: 0,
    reasoning: 0,
  });
  assert.equal(totalTokens(z), 0);
  assert.equal(
    totalTokens({
      input: 1,
      output: 2,
      cache_creation: 3,
      cache_read: 4,
      reasoning: 5,
    }),
    15,
  );
});
