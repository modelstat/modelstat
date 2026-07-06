import { existsSync } from "node:fs";
import { hostname } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { config as loadDotenv } from "dotenv";
import { type DeviceIdentity, loadIdentity, saveIdentity, updateIdentity } from "./identity.js";
import {
  type FileCursor,
  parseSummarizerMode,
  runtimeState,
  type SummarizerMode,
  statePath,
} from "./runtime-state.js";

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

/** Production API. Dev overrides via DAEMON_API_URL (set in .env). */
const DEFAULT_API_URL = "https://modelstat.ai";

/** Legacy default that `@modelstat/daemon@0.0.7` persisted to disk on
 * first run. If we see this exact string in the stored state today
 * and no env override is set, treat it as unset and use the new
 * default. Prevents the "upgraded but still points at localhost" trap. */
const LEGACY_LOCALHOST_API = "http://localhost:3010";

// Identity is backed by ~/.modelstat/identity.json (canonical since 0.0.23).
// Runtime state (apiUrl / cursors / counters) is backed by
// ~/.modelstat/state.json (see ./runtime-state.ts) — both under the single
// `modelstatHome()` so there's exactly one daemon-state location per machine,
// identical on every OS and relocatable via MODELSTAT_HOME. The old `conf`
// store (and its OS/name-specific path) is gone; the pre-0.0.23 conf→identity
// migration went with it (identity.json has long been the source of truth).
let cachedIdentity: DeviceIdentity | null = loadIdentity();

function writeThrough(patch: Partial<DeviceIdentity>): void {
  if (!cachedIdentity) {
    // Can't update before the identity exists — setBearer/setDeviceId
    // etc. are called in sequence right after selfRegister(). Don't
    // allow partial writes; require saveFreshIdentity() to seed.
    throw new Error("config: no identity yet — call state.saveFreshIdentity() first");
  }
  cachedIdentity = { ...cachedIdentity, ...patch };
  updateIdentity(patch);
}

export const state = {
  /** Resolution order: env var → stored value (if user ran `setApiUrl`
   * or paired pre-0.0.8) → production default. The legacy localhost
   * value is ignored so upgrades from 0.0.7 self-heal. */
  get apiUrl(): string {
    if (process.env.DAEMON_API_URL) return process.env.DAEMON_API_URL;
    const stored = runtimeState.getApiUrl();
    if (stored && stored !== LEGACY_LOCALHOST_API) return stored;
    return DEFAULT_API_URL;
  },
  setApiUrl(v: string): void {
    runtimeState.setApiUrl(v);
  },
  /** True when `apiUrl` resolves to the baked-in production default with no
   * explicit override (no `DAEMON_API_URL`, no operator-set stored URL). Used
   * to refuse silent prod self-register from CI/non-interactive environments
   * so ephemeral runners don't pile up unclaimed device rows. */
  get isProdDefaultApi(): boolean {
    if (process.env.DAEMON_API_URL) return false;
    const stored = runtimeState.getApiUrl();
    if (stored && stored !== LEGACY_LOCALHOST_API) return false;
    return true;
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

  // ── Runtime state: backed by ~/.modelstat/state.json ───────────

  getCursor(path: string): FileCursor | undefined {
    return runtimeState.getCursor(path);
  },
  setCursor(path: string, v: FileCursor): void {
    runtimeState.setCursor(path, v);
  },
  /** Drop every per-file cursor so the next scan re-reads every
   * JSONL from byte 0. Called when the local processing-pipeline
   * version bumps (see processing-version.ts) or via the
   * `modelstat reset` CLI command. */
  wipeCursors(): void {
    runtimeState.wipeCursors();
  },

  /** Lifetime tally of segments uploaded from this machine. Survives
   * daemon restarts so "total sent" keeps climbing across reboots. */
  get segmentsSent(): number {
    return runtimeState.getSegmentsSent();
  },
  /** Add to the lifetime tally and return the new total. */
  bumpSegmentsSent(n: number): number {
    return runtimeState.bumpSegmentsSent(n);
  },

  get processingVersion(): number | null {
    return runtimeState.getProcessingVersion();
  },
  setProcessingVersion(v: number): void {
    runtimeState.setProcessingVersion(v);
  },

  // ── Summariser mode: where each session gets summarised ────────
  // Resolution mirrors `apiUrl`: an explicit env override wins (handy for
  // CI / scripted installs), else the value chosen at install and persisted
  // to state.json, else the baked-in default (cloud). Redaction stays
  // client-side in every mode — only the summarisation LOCATION changes.

  /** The effective summariser mode. `MODELSTAT_SUMMARIZER_MODE` overrides the
   * persisted choice; an unset/garbage env value falls through to it. */
  get summarizerMode(): SummarizerMode {
    const override = parseSummarizerMode(process.env.MODELSTAT_SUMMARIZER_MODE);
    if (override) return override;
    return runtimeState.getSummarizerMode();
  },
  setSummarizerMode(v: SummarizerMode): void {
    runtimeState.setSummarizerMode(v);
  },

  /** The self-hosted endpoint (base URL + model). `MODELSTAT_LLM_BASE_URL` /
   * `MODELSTAT_LLM_MODEL` override the persisted values when both are present
   * (so a self-hosted install can be driven entirely from the environment). */
  get selfHosted(): { url: string; model: string } {
    const envUrl = process.env.MODELSTAT_LLM_BASE_URL?.trim();
    const envModel = process.env.MODELSTAT_LLM_MODEL?.trim();
    if (envUrl && envModel) return { url: envUrl, model: envModel };
    return runtimeState.getSelfHosted();
  },
  setSelfHosted(url: string, model: string): void {
    runtimeState.setSelfHosted(url, model);
  },

  /** True when the persisted (or default) mode isn't overridden by
   * `MODELSTAT_SUMMARIZER_MODE`. Lets `modelstat mode` warn that an env var is
   * masking the stored choice. */
  get summarizerModeIsEnvOverridden(): boolean {
    return parseSummarizerMode(process.env.MODELSTAT_SUMMARIZER_MODE) !== null;
  },

  get storePath(): string {
    return statePath();
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
