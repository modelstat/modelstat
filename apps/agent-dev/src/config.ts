import { config as loadDotenv } from "dotenv";
import Conf from "conf";
import { existsSync } from "node:fs";
import { hostname } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  loadIdentity,
  saveIdentity,
  updateIdentity,
  type DeviceIdentity,
} from "./identity.js";

// Walk up from this file to find .env (same pattern as api/worker).
const here = dirname(fileURLToPath(import.meta.url));
for (let d = here, i = 0; i < 8; i++, d = resolve(d, "..")) {
  const candidate = resolve(d, ".env");
  if (existsSync(candidate)) {
    loadDotenv({ path: candidate });
    break;
  }
  if (d === "/") break;
}

interface AgentState {
  apiUrl: string;
  /** Per-install runtime state — cursors. Identity fields (device
   * uuid/id/bearer/claim) moved to ~/.modelstat/identity.json in
   * 0.0.23; the fields below are kept in the type for one-shot
   * migration on first run of a post-0.0.22 binary. */
  bearerToken: string | null;
  deviceId: string | null;
  deviceUuid: string | null;
  claimCode: string | null;
  claimUrl: string | null;
  userEmail: string | null;
  defaultOrgId: string | null;
  cursor: Record<string, { size: number; mtime: number; tailHash: string }>;
  /** Lifetime count of segments successfully uploaded from this
   * machine. Kept locally (never read back from the server) so the
   * menu-bar tray can show a running "total sent" without an
   * authenticated round-trip. Best-effort: incremented after each
   * confirmed batch upload, so a hard crash mid-batch may lose the
   * last increment. Lossy by design — it's a feel-good counter, not
   * an accounting ledger. */
  segmentsSent: number;
  /** Sticky marker for the local processing-pipeline version that
   * last produced cursors. When the agent's compiled-in
   * PROCESSING_VERSION is higher than this, the daemon wipes all
   * cursors at startup so every session gets re-summarised by the
   * new pipeline. NULL = pre-versioning install (treat as stale). */
  processingVersion: number | null;
}

/** Production API. Dev overrides via AGENT_API_URL (set in .env). */
const DEFAULT_API_URL = "https://modelstat.ai";

/** Legacy default that `@modelstat/agent@0.0.7` persisted to disk on
 * first run. If we see this exact string in the stored state today
 * and no env override is set, treat it as unset and use the new
 * default. Prevents the "upgraded but still points at localhost" trap. */
const LEGACY_LOCALHOST_API = "http://localhost:3010";

const store = new Conf<AgentState>({
  projectName: "modelstat-agent-dev",
  defaults: {
    // Intentionally empty — the apiUrl getter below computes this
    // from env + stored value + DEFAULT_API_URL. Keeping the stored
    // default empty avoids freezing a stale value into the config
    // file the way 0.0.7 did.
    apiUrl: "",
    bearerToken: null,
    deviceId: null,
    deviceUuid: null,
    claimCode: null,
    claimUrl: null,
    userEmail: null,
    defaultOrgId: null,
    cursor: {},
    segmentsSent: 0,
    processingVersion: null,
  },
});

/**
 * Read identity fields from ~/.modelstat/identity.json (canonical
 * post-0.0.23). If missing, load-with-migration falls back to the
 * legacy `conf` store on first run so an upgrade from 0.0.22 doesn't
 * lose the user's enrollment. After migration the conf copy is
 * cleared so there's exactly one source of truth.
 */
function migrateFromConf(): {
  bearerToken: string | null;
  deviceId: string | null;
  deviceUuid: string | null;
  claimCode: string | null;
  claimUrl: string | null;
  userEmail: string | null;
  defaultOrgId: string | null;
} | null {
  const bearer = store.get("bearerToken");
  const deviceUuid = store.get("deviceUuid");
  const deviceId = store.get("deviceId");
  if (!bearer || !deviceUuid || !deviceId) return null;
  return {
    bearerToken: bearer,
    deviceId,
    deviceUuid,
    claimCode: store.get("claimCode"),
    claimUrl: store.get("claimUrl"),
    userEmail: store.get("userEmail"),
    defaultOrgId: store.get("defaultOrgId"),
  };
}

function clearConfIdentity(): void {
  // After migration, strip the fields from conf so future reads always
  // resolve to identity.json. Leave cursor + apiUrl alone.
  store.set("bearerToken", null);
  store.set("deviceId", null);
  store.set("deviceUuid", null);
  store.set("claimCode", null);
  store.set("claimUrl", null);
}

// Cache the identity in-memory so we don't re-read the file on every
// getter access. Writes go through the setters below which update
// both the cache and the file.
let cachedIdentity: DeviceIdentity | null = (() => {
  const had = store.get("bearerToken") !== null;
  const id = loadIdentity(migrateFromConf);
  // If we just migrated, scrub conf so there's one source of truth.
  if (id && had && !store.path.includes("fallback")) {
    clearConfIdentity();
  }
  return id;
})();

function writeThrough(patch: Partial<DeviceIdentity>): void {
  if (!cachedIdentity) {
    // Can't update before the identity exists — setBearer/setDeviceId
    // etc. are called in sequence right after selfRegister(). Don't
    // allow partial writes; require saveFreshIdentity() to seed.
    throw new Error(
      "config: no identity yet — call state.saveFreshIdentity() first",
    );
  }
  cachedIdentity = { ...cachedIdentity, ...patch };
  updateIdentity(patch);
}

export const state = {
  /** Resolution order: env var → stored value (if user ran `setApiUrl`
   * or paired pre-0.0.8) → production default. The legacy localhost
   * value is ignored so upgrades from 0.0.7 self-heal. */
  get apiUrl(): string {
    if (process.env.AGENT_API_URL) return process.env.AGENT_API_URL;
    const stored = store.get("apiUrl");
    if (stored && stored !== LEGACY_LOCALHOST_API) return stored;
    return DEFAULT_API_URL;
  },
  setApiUrl(v: string): void {
    store.set("apiUrl", v);
  },

  // ── Identity: backed by ~/.modelstat/identity.json ─────────────

  /** Seed a fresh identity after a successful self-register. Writes
   * the file atomically; use `state.backupAndReset()` first if
   * overwriting an existing identity. */
  saveFreshIdentity(meta: {
    deviceUuid: string;
    deviceId: string;
    bearerToken: string;
    claimCode: string | null;
    claimUrl: string | null;
  }): void {
    const id: DeviceIdentity = {
      deviceUuid: meta.deviceUuid,
      deviceId: meta.deviceId,
      bearerToken: meta.bearerToken,
      claimCode: meta.claimCode,
      claimUrl: meta.claimUrl,
      hostname: hostname(),
      createdAt: new Date().toISOString(),
      userEmail: null,
      defaultOrgId: null,
    };
    saveIdentity(id);
    cachedIdentity = id;
  },

  get identity(): DeviceIdentity | null {
    return cachedIdentity;
  },

  get bearer(): string | null {
    return cachedIdentity?.bearerToken ?? null;
  },
  setBearer(v: string | null): void {
    if (v === null) {
      // Explicit wipe — used by disconnect flows. Caller is
      // responsible for any backup.
      cachedIdentity = null;
      return;
    }
    writeThrough({ bearerToken: v });
  },

  get deviceId(): string | null {
    return cachedIdentity?.deviceId ?? null;
  },

  get deviceUuid(): string | null {
    return cachedIdentity?.deviceUuid ?? null;
  },

  get claimCode(): string | null {
    return cachedIdentity?.claimCode ?? null;
  },
  setClaimCode(v: string | null): void {
    if (!cachedIdentity) return;
    writeThrough({ claimCode: v });
  },

  get claimUrl(): string | null {
    return cachedIdentity?.claimUrl ?? null;
  },
  setClaimUrl(v: string | null): void {
    if (!cachedIdentity) return;
    writeThrough({ claimUrl: v });
  },

  get userEmail(): string | null {
    return cachedIdentity?.userEmail ?? null;
  },
  setUserEmail(v: string): void {
    if (!cachedIdentity) return;
    writeThrough({ userEmail: v });
  },

  // ── Runtime state: stays in conf ───────────────────────────────

  getCursor(
    path: string,
  ): { size: number; mtime: number; tailHash: string } | undefined {
    return store.get("cursor")[path];
  },
  setCursor(
    path: string,
    v: { size: number; mtime: number; tailHash: string },
  ): void {
    const map = store.get("cursor");
    map[path] = v;
    store.set("cursor", map);
  },
  /** Drop every per-file cursor so the next scan re-reads every
   * JSONL from byte 0. Called when the local processing-pipeline
   * version bumps (see processing-version.ts) or via the
   * `modelstat rescan` CLI command. */
  wipeCursors(): void {
    store.set("cursor", {});
  },

  /** Lifetime tally of segments uploaded from this machine. Survives
   * daemon restarts so "total sent" keeps climbing across reboots. */
  get segmentsSent(): number {
    return store.get("segmentsSent") ?? 0;
  },
  /** Add to the lifetime tally and return the new total. Persisted
   * synchronously (same cadence as cursor writes during a scan). */
  bumpSegmentsSent(n: number): number {
    const next = (store.get("segmentsSent") ?? 0) + n;
    store.set("segmentsSent", next);
    return next;
  },

  get processingVersion(): number | null {
    return store.get("processingVersion");
  },
  setProcessingVersion(v: number): void {
    store.set("processingVersion", v);
  },

  get storePath(): string {
    return store.path;
  },
};

export function machineId(): string {
  // Good enough for dev. A production build will use a stable platform id.
  return `dev-${hostname()}`;
}

export function defaultUserEmail(): string {
  // Dev-only fallback used by the `register` (email-shortcut) command
  // when running against a local API. Override with AGENT_USER_EMAIL
  // or `state.userEmail` for a real email.
  return process.env.AGENT_USER_EMAIL ?? state.userEmail ?? "user@localhost";
}
