/**
 * Stable, persisted device identity for the MCP's registration.
 *
 * The MCP registers a DISTINCT logical device from the daemon (a user may run
 * either or both — see auth.ts), but it must be STABLE across re-runs: if the
 * browser claim times out, or mcp-auth.json is lost, re-running the claim flow
 * must dedupe onto the SAME server device row instead of piling up unclaimed
 * duplicates (the "one machine → three dashboard rows" pathology the daemon's
 * machine-key.ts was written to kill).
 *
 * The register door (POST /v1/tokens) dedupes on `fingerprint.machine_id`, so
 * we send a stable id there. We persist it to ~/.modelstat/mcp-device.json the
 * first time it is needed — BEFORE any claim succeeds — so even a timed-out,
 * never-claimed registration reuses the same id on the next attempt.
 *
 * This is the MCP analogue of the daemon's machine key. We keep it
 * home-dir-stable (a persisted random id) rather than hardware-derived: the
 * MCP's bearer already lives under ~/.modelstat, so this id is no more fragile
 * than the auth it anchors, and it stays a SEPARATE id from the daemon's by
 * construction (own file, own random value) — so the MCP never dedupes onto
 * the daemon's device row.
 */
import { randomBytes, randomUUID } from "node:crypto";
import { mkdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { modelstatHome } from "./state.js";

export interface DeviceIdentity {
  /** Server dedupe anchor — a stable, opaque 64-hex id (sent as
   * `fingerprint.machine_id`). Distinct from the daemon's by construction. */
  machineId: string;
  /** This logical device's UUID (sent as the top-level `device_uuid`). */
  deviceUuid: string;
}

let cached: DeviceIdentity | null = null;

/** ~/.modelstat/mcp-device.json — persisted independently of mcp-auth.json so
 * the id survives a claim that never completed. */
function deviceIdPath(): string {
  return join(modelstatHome(), "mcp-device.json");
}

function read(): DeviceIdentity | null {
  try {
    const rec = JSON.parse(readFileSync(deviceIdPath(), "utf8")) as Record<string, unknown>;
    if (typeof rec.machineId === "string" && typeof rec.deviceUuid === "string") {
      return { machineId: rec.machineId, deviceUuid: rec.deviceUuid };
    }
  } catch {
    // absent or malformed — mint a fresh one
  }
  return null;
}

function persist(id: DeviceIdentity): void {
  const target = deviceIdPath();
  const tmp = `${target}.tmp-${process.pid}`;
  try {
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(tmp, JSON.stringify(id), { encoding: "utf8", mode: 0o600 });
    renameSync(tmp, target);
  } catch {
    // Best effort — an unpersisted id is still stable for this process, so the
    // current claim attempt works; only cross-run dedupe is lost.
    try {
      unlinkSync(tmp);
    } catch {
      /* ignore */
    }
  }
}

/**
 * The MCP's stable device identity — read from disk, or minted and persisted on
 * first use so repeated registrations dedupe to one server row. Memoised for
 * the process.
 */
export function deviceIdentity(): DeviceIdentity {
  if (cached) return cached;
  const existing = read();
  if (existing) {
    cached = existing;
    return cached;
  }
  const fresh: DeviceIdentity = {
    machineId: randomBytes(32).toString("hex"),
    deviceUuid: randomUUID(),
  };
  persist(fresh);
  cached = fresh;
  return fresh;
}

/** Test-only: drop the memoised identity so a test can exercise a fresh home. */
export function _resetDeviceIdentityCache(): void {
  cached = null;
}
