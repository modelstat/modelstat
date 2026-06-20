/**
 * Device identity — stable across reinstalls.
 *
 * Lives at ~/.modelstat/identity.json. Contains the fields a fresh
 * install needs to resume a previous enrollment without triggering
 * a new self-register (which would create a ghost device row in the
 * user's account).
 *
 * Lives under the single daemon home (`modelstatHome()`, default `~/.modelstat`,
 * relocatable via MODELSTAT_HOME) alongside `state.json` — see ./paths.ts. The
 * identity file holds a long-lived bearer, so it gets explicit chmod 0600.
 * Runtime bookkeeping (cursors, API-URL override) lives in the sibling
 * `state.json` (./runtime-state.ts); both share one location so state can't fork.
 */

import {
  chmodSync,
  mkdirSync,
  readFileSync,
  renameSync,
  writeFileSync,
  existsSync,
} from "node:fs";
import { hostname as osHostname } from "node:os";
import { join } from "node:path";
import { modelstatHome } from "./paths.js";

export interface DeviceIdentity {
  /** Daemon-generated UUIDv7. Stable across reinstalls. */
  deviceUuid: string;
  /** Server UUID returned from /v1/devices/self-register. */
  deviceId: string;
  /** Long-lived Bearer (ds_live_…). Rotate via /v1/devices/me/rotate-secret. */
  bearerToken: string;
  /** Claim code surfaced to the user. Once the device is claimed, this stays
   * non-null but the server returns `status: "claimed"` on /devices/me. */
  claimCode: string | null;
  claimUrl: string | null;
  /** Hostname at the moment of self-register. Shown in the "reuse
   * existing device?" prompt so the user can spot if they're about to
   * reuse an identity that belongs to a different machine. */
  hostname: string;
  /** ISO timestamp. For audit + UI. */
  createdAt: string;
  /** Optional — set once the daemon discovers from /devices/me that the
   * device is claimed. Just a display convenience. */
  userEmail?: string | null;
  /** Default routing org snapshot. Display-only. */
  defaultOrgId?: string | null;
}

const IDENTITY_FILE = join(modelstatHome(), "identity.json");

function ensureRoot(): void {
  mkdirSync(modelstatHome(), { recursive: true, mode: 0o700 });
}

/** Atomic write + chmod 0600. Refuses to overwrite silently — caller
 * should call `backupIdentity()` first if an existing file is present
 * and represents a different identity. */
function writeAtomic(meta: DeviceIdentity): void {
  ensureRoot();
  const tmp = `${IDENTITY_FILE}.${process.pid}.tmp`;
  writeFileSync(tmp, JSON.stringify(meta, null, 2), { mode: 0o600 });
  renameSync(tmp, IDENTITY_FILE);
  // rename preserves tmp's mode; extra chmod ensures it in case
  // umask altered it on some platforms.
  try {
    chmodSync(IDENTITY_FILE, 0o600);
  } catch {
    /* best effort */
  }
}

export function identityPath(): string {
  return IDENTITY_FILE;
}

export function hasIdentityFile(): boolean {
  return existsSync(IDENTITY_FILE);
}

function parseFile(): DeviceIdentity | null {
  try {
    const raw = readFileSync(IDENTITY_FILE, "utf8");
    const obj = JSON.parse(raw) as Partial<DeviceIdentity>;
    if (!obj.deviceUuid || !obj.deviceId || !obj.bearerToken) {
      return null;
    }
    return {
      deviceUuid: obj.deviceUuid,
      deviceId: obj.deviceId,
      bearerToken: obj.bearerToken,
      claimCode: obj.claimCode ?? null,
      claimUrl: obj.claimUrl ?? null,
      hostname: obj.hostname ?? osHostname(),
      createdAt: obj.createdAt ?? new Date().toISOString(),
      userEmail: obj.userEmail ?? null,
      defaultOrgId: obj.defaultOrgId ?? null,
    };
  } catch {
    return null;
  }
}

/** Read the canonical identity from `identity.json`, or null if absent. */
export function loadIdentity(): DeviceIdentity | null {
  return parseFile();
}

/** Persist the current identity. Writes atomically; caller is
 * responsible for deciding whether an existing file should be backed
 * up first (e.g., user chose `--fresh`). */
export function saveIdentity(meta: DeviceIdentity): void {
  writeAtomic(meta);
}

/**
 * Rename the current identity file to
 * `~/.modelstat/identity.json.bak-<ISO timestamp>`. Returns the backup
 * path if one was made, or null if no file existed to back up.
 */
export function backupIdentity(): string | null {
  if (!existsSync(IDENTITY_FILE)) return null;
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const dest = `${IDENTITY_FILE}.bak-${stamp}`;
  renameSync(IDENTITY_FILE, dest);
  return dest;
}

/** Partial update — read + merge + write. Use for stamping
 * userEmail / defaultOrgId / claim state changes without touching
 * the device_uuid + bearer. */
export function updateIdentity(patch: Partial<DeviceIdentity>): DeviceIdentity | null {
  const current = parseFile();
  if (!current) return null;
  const merged: DeviceIdentity = { ...current, ...patch };
  writeAtomic(merged);
  return merged;
}
