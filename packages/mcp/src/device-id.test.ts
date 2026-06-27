/**
 * Device-identity tests — the MCP's register dedupe anchor must be machine-
 * stable: minted + persisted on first use, then re-read so repeated claim
 * attempts reuse one server device row. Deterministic (temp MODELSTAT_HOME).
 */
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, test } from "node:test";
import { _resetDeviceIdentityCache, deviceIdentity } from "./device-id.js";

const HEX64 = /^[0-9a-f]{64}$/;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

let savedHome: string | undefined;

beforeEach(() => {
  savedHome = process.env.MODELSTAT_HOME;
  process.env.MODELSTAT_HOME = mkdtempSync(join(tmpdir(), "modelstat-mcp-dev-"));
  _resetDeviceIdentityCache();
});

afterEach(() => {
  if (savedHome === undefined) delete process.env.MODELSTAT_HOME;
  else process.env.MODELSTAT_HOME = savedHome;
  _resetDeviceIdentityCache();
});

test("mints a stable id and persists it to mcp-device.json", () => {
  const home = process.env.MODELSTAT_HOME as string;
  const id = deviceIdentity();
  assert.match(id.machineId, HEX64);
  assert.match(id.deviceUuid, UUID);

  const onDisk = JSON.parse(readFileSync(join(home, "mcp-device.json"), "utf8"));
  assert.equal(onDisk.machineId, id.machineId);
  assert.equal(onDisk.deviceUuid, id.deviceUuid);
});

test("memoises within a process", () => {
  assert.equal(deviceIdentity(), deviceIdentity());
});

test("re-reads a persisted identity instead of minting a new one", () => {
  const home = process.env.MODELSTAT_HOME as string;
  const known = { machineId: "a".repeat(64), deviceUuid: "11111111-1111-1111-1111-111111111111" };
  writeFileSync(join(home, "mcp-device.json"), JSON.stringify(known));

  const id = deviceIdentity();
  assert.deepEqual(id, known);
});
