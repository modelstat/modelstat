/**
 * State / auth-resolution tests — the env fast path, the mcp-auth persist
 * round-trip, and path resolution under MODELSTAT_HOME. We avoid asserting the
 * daemon-CLI branches (those depend on a `modelstat` binary being installed +
 * paired on the host); the env + file paths are deterministic.
 */
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, test } from "node:test";
import { loadState, mcpAuthPath, modelstatHome, persistMcpAuth } from "./state.js";

const saved: Record<string, string | undefined> = {};
const KEYS = [
  "MODELSTAT_HOME",
  "MODELSTAT_TOKEN",
  "MODELSTAT_API_URL",
  "DAEMON_API_URL",
  "MODELSTAT_STATE_FILE",
];

beforeEach(() => {
  for (const k of KEYS) saved[k] = process.env[k];
  for (const k of KEYS) delete process.env[k];
  process.env.MODELSTAT_HOME = mkdtempSync(join(tmpdir(), "modelstat-mcp-"));
});

afterEach(() => {
  for (const k of KEYS) {
    if (saved[k] === undefined) delete process.env[k];
    else process.env[k] = saved[k];
  }
});

test("modelstatHome honours MODELSTAT_HOME; mcpAuthPath sits under it", () => {
  const home = process.env.MODELSTAT_HOME as string;
  assert.equal(modelstatHome(), home);
  assert.equal(mcpAuthPath(), join(home, "mcp-auth.json"));
});

test("MODELSTAT_TOKEN is the fast path (no daemon needed)", () => {
  process.env.MODELSTAT_TOKEN = "tok_env_123";
  process.env.MODELSTAT_API_URL = "https://example.test";
  const s = loadState();
  assert.equal(s.bearer, "tok_env_123");
  assert.equal(s.apiUrl, "https://example.test");
  assert.equal(s.source, "env");
});

test("a legacy localhost api url is ignored in favour of prod default", () => {
  process.env.MODELSTAT_TOKEN = "tok";
  process.env.MODELSTAT_API_URL = "http://localhost:3010";
  assert.equal(loadState().apiUrl, "https://modelstat.ai");
});

test("persistMcpAuth round-trips through mcp-auth.json (daemon field names)", () => {
  persistMcpAuth({ bearer: "tok_mcp", deviceId: "dev_1", deviceUuid: "uuid_1" });
  const onDisk = JSON.parse(readFileSync(mcpAuthPath(), "utf8"));
  assert.equal(onDisk.bearerToken, "tok_mcp");
  assert.equal(onDisk.deviceId, "dev_1");
  assert.equal(onDisk.deviceUuid, "uuid_1");
  assert.equal(onDisk.source, "mcp-browser-claim");
});

test("with no env + a fresh home, the persisted mcp bearer is picked up", () => {
  // Guard: only meaningful when the host has no paired `modelstat` daemon that
  // `loadState` would resolve first. If a daemon token resolves, skip the
  // assertion (the fast path legitimately wins).
  persistMcpAuth({ bearer: "tok_persisted", deviceId: "dev_p" });
  const s = loadState();
  if (s.source === "mcp-auth") {
    assert.equal(s.bearer, "tok_persisted");
    assert.equal(s.deviceId, "dev_p");
  } else {
    // A real daemon is installed + paired on this host — the daemon source
    // correctly takes priority. Nothing to assert about our file in that case.
    assert.ok(["daemon-token", "daemon-identity"].includes(s.source));
  }
});
