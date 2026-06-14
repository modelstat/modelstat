/**
 * Regression tests for the machine-key / deterministic-UUID anchor.
 *
 * These pin the property that makes device registration idempotent:
 * the SAME machine key always yields the SAME device UUID, and that
 * UUID is a valid RFC 9562 v5 UUID. This is what stops one physical
 * machine from becoming multiple dashboard device rows after a
 * reinstall, a package-manager switch, or a ~/.modelstat wipe.
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import { deviceUuidFromMachineKey, machineKey, machineKeySource } from "./machine-key.js";

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

test("deviceUuidFromMachineKey is deterministic for a given key", () => {
  const key = "a".repeat(64);
  const a = deviceUuidFromMachineKey(key);
  const b = deviceUuidFromMachineKey(key);
  assert.equal(a, b, "same key must always derive the same UUID");
});

test("derived UUID is a well-formed v5 UUID (version + variant bits set)", () => {
  const u = deviceUuidFromMachineKey("b".repeat(64));
  assert.match(u, UUID_RE);
  // version nibble (1st char of 3rd group) must be 5
  assert.equal(u[14], "5", `expected version 5, got ${u}`);
  // variant nibble (1st char of 4th group) must be one of 8,9,a,b
  assert.ok(["8", "9", "a", "b"].includes(u[19]!), `expected variant 10xx, got ${u}`);
});

test("different machine keys derive different UUIDs", () => {
  const a = deviceUuidFromMachineKey("c".repeat(64));
  const b = deviceUuidFromMachineKey("d".repeat(64));
  assert.notEqual(a, b);
});

test("MODELSTAT_DEVICE_SALT-style suffixing yields a distinct but still-deterministic UUID", () => {
  const base = "e".repeat(64);
  const salted1 = deviceUuidFromMachineKey(`${base}:ci-runner-2`);
  const salted2 = deviceUuidFromMachineKey(`${base}:ci-runner-2`);
  assert.equal(salted1, salted2, "salted derivation must still be deterministic");
  assert.notEqual(salted1, deviceUuidFromMachineKey(base), "salt must change the UUID");
});

test("machineKey() is a stable 64-char hex digest, memoised across calls", () => {
  const k1 = machineKey();
  const k2 = machineKey();
  assert.equal(k1, k2);
  assert.match(k1, /^[0-9a-f]{64}$/);
});

test("machineKeySource() reports a known provenance", () => {
  const src = machineKeySource();
  assert.ok(
    ["macos-ioplatform", "linux-machine-id", "windows-guid", "fallback-file"].includes(src),
    `unexpected source: ${src}`,
  );
});

test("end-to-end: machineKey() → deviceUuidFromMachineKey() is reproducible", () => {
  // Simulates a fresh install re-deriving identity with no identity.json:
  // it must reproduce the exact UUID every run on this machine.
  const first = deviceUuidFromMachineKey(machineKey());
  const second = deviceUuidFromMachineKey(machineKey());
  assert.equal(first, second);
  assert.match(first, UUID_RE);
});
