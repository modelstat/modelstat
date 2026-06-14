/**
 * Device identity — stable across reinstalls.
 *
 * Lives at ~/.modelstat/identity.json. Contains the fields a fresh
 * install needs to resume a previous enrollment without triggering
 * a new self-register (which would create a ghost device row in the
 * user's account).
 *
 * Separate from the `conf` store because:
 *   - `conf` lives under ~/Library/Preferences/ (macOS) or
 *     ~/.config/ (linux), whose paths are easy for users to nuke
 *     during debugging without realizing what they're deleting.
 *   - Users' mental model is "my modelstat config lives in
 *     ~/.modelstat/" (that's where the CLI installs the binary and
 *     writes logs + the daemon lockfile too).
 *   - The identity file holds a long-lived bearer — we want explicit
 *     chmod 0600 so nothing else on the machine can read it.
 *
 * The `conf` store stays around for runtime state: file cursors,
 * overridden API URL, etc. Things that are per-install or
 * per-session and don't need to survive a nuke.
 *
 * Migration: `loadIdentity()` first checks the file; if missing but
 * the legacy `conf` store contains identity fields (pre-0.0.23
 * installs), it migrates them once and deletes the conf copy so
 * we have exactly one source of truth going forward.
 */

import {
  chmodSync,
  mkdirSync,
  readFileSync,
  renameSync,
  writeFileSync,
  existsSync,
} from "node:fs";
import { homedir, hostname as osHostname } from "node:os";
import { join } from "node:path";

export interface DeviceIdentity {
  /** Agent-generated UUIDv7. Stable across reinstalls. */
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
  /** Optional — set once the agent discovers from /devices/me that the
   * device is claimed. Just a display convenience. */
  userEmail?: string | null;
  /** Default routing org snapshot. Display-only. */
  defaultOrgId?: string | null;
}

/**
 * Legacy slots the conf store used pre-0.0.23. Duplicated here (not
 * imported from ./config.ts) so this module doesn't create a cycle.
 */
interface LegacyConfShape {
  bearerToken?: string | null;
  deviceId?: string | null;
  deviceUuid?: string | null;
  claimCode?: string | null;
  claimUrl?: string | null;
  userEmail?: string | null;
  defaultOrgId?: string | null;
}

const ROOT = join(homedir(), ".modelstat");
const IDENTITY_FILE = join(ROOT, "identity.json");

function ensureRoot(): void {
  mkdirSync(ROOT, { recursive: true, mode: 0o700 });
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

/**
 * Read the canonical identity. If the file is missing but the legacy
 * `conf` store has identity fields (pre-0.0.23 install), migrate them
 * across once. Returns null if no identity is present anywhere.
 */
export function loadIdentity(migrateFromConf?: () => LegacyConfShape | null): DeviceIdentity | null {
  const fromFile = parseFile();
  if (fromFile) return fromFile;

  if (!migrateFromConf) return null;
  const legacy = migrateFromConf();
  if (!legacy) return null;
  if (!legacy.deviceUuid || !legacy.deviceId || !legacy.bearerToken) {
    return null;
  }
  const migrated: DeviceIdentity = {
    deviceUuid: legacy.deviceUuid,
    deviceId: legacy.deviceId,
    bearerToken: legacy.bearerToken,
    claimCode: legacy.claimCode ?? null,
    claimUrl: legacy.claimUrl ?? null,
    hostname: osHostname(),
    createdAt: new Date().toISOString(),
    userEmail: legacy.userEmail ?? null,
    defaultOrgId: legacy.defaultOrgId ?? null,
  };
  writeAtomic(migrated);
  return migrated;
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
