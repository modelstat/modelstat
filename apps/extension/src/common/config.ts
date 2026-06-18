/**
 * Build-time + runtime config for the extension. The API URL is a
 * build-time default but user-overridable in the Options page (handy
 * for self-hosted / staging). All other values are compile-time
 * constants — changing them requires a release.
 */

// Prod serves both the API (/v1/*) and the SPA from modelstat.ai on
// a single origin. No api.modelstat.ai subdomain.
export const DEFAULT_API_URL = import.meta.env.DEV
  ? "http://localhost:3010"
  : "https://modelstat.ai";

/** Dashboard origin — where /connect, /dashboard/devices, etc. live. */
export const DASHBOARD_URL = import.meta.env.DEV
  ? "http://localhost:5173"
  : "https://modelstat.ai";

export const ADAPTER_POLL_INTERVAL_MS = 15 * 60 * 1000; // 15 min
// Upload cadence: we don't batch on a fixed tick anymore. The queue
// drain runs on a short interval, but only ships a session's events
// once that session has been quiet for at least SESSION_DEBOUNCE_MS
// (5–10 s — user-configurable). Finalised sessions go first, ordered
// most-recent → oldest, which matches the dashboard's live priority.
// Ingest sizing is the shared cross-companion contract — sourced from one
// place (@modelstat/companion-core/config) so the extension and the CLI use
// the same batch cadence + size instead of drifting apart.
export {
  FORCE_SHIP_THRESHOLD,
  INGEST_BATCH_INTERVAL_MS,
  INGEST_BATCH_MAX_EVENTS,
  SESSION_DEBOUNCE_MS,
} from "@modelstat/companion-core/config";
export const MESSAGE_FINALISE_WINDOW_MS = 30_000; // two-phase commit window
export const MESSAGE_FINALISE_DOM_QUIET_MS = 2_000; // stream-end + DOM stable
export const SSE_FLUSH_INTERVAL_MS = 50;

// First-impression fast path. Until a device's first successful ship, the very
// first session bypasses the normal finalise/flush cadence so data reaches the
// dashboard in seconds instead of tens of seconds. Steady state is untouched.
export const EAGER_FINALISE_QUIET_MS = 1_500; // finalise a stream-ended msg this soon after its last update
export const FIRST_IMPRESSION_QUIET_MS = 2_000; // debounce before the eager finalise+flush kick

export const AGENT_VERSION = `modelstat-extension@${chrome.runtime.getManifest().version}`;

// Bridge between MAIN and ISOLATED content script worlds. Random per
// page-load to prevent external sites from spoofing our messages.
export function newNonce(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

export const BRIDGE_TAG = "__modelstat_bridge__" as const;

/**
 * RFC 9562 UUIDv7 — time-ordered UUID, required by
 * /v1/devices/self-register. 48 bits ms timestamp + 12 bits
 * sub-ms randomness + 62 bits random. Works anywhere globalThis.crypto
 * does (i.e. everywhere MV3 runs).
 */
export function uuidv7(): string {
  const ms = BigInt(Date.now());
  const rand = crypto.getRandomValues(new Uint8Array(10));
  const bytes = new Uint8Array(16);
  // 48-bit big-endian timestamp
  bytes[0] = Number((ms >> 40n) & 0xffn);
  bytes[1] = Number((ms >> 32n) & 0xffn);
  bytes[2] = Number((ms >> 24n) & 0xffn);
  bytes[3] = Number((ms >> 16n) & 0xffn);
  bytes[4] = Number((ms >> 8n) & 0xffn);
  bytes[5] = Number(ms & 0xffn);
  bytes[6] = 0x70 | (rand[0]! & 0x0f); // version 7
  bytes[7] = rand[1]!;
  bytes[8] = 0x80 | (rand[2]! & 0x3f); // variant 10
  bytes[9] = rand[3]!;
  for (let i = 4; i < 10; i++) bytes[10 + (i - 4)] = rand[i]!;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return (
    `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20, 32)}`
  );
}

/**
 * Crockford-base32 ULID generator that works in MV3 service workers.
 *
 * The `ulid` npm package reads `window.crypto` at import time, which
 * throws in a service worker context (no window). This drop-in uses
 * globalThis.crypto.getRandomValues, which is available everywhere
 * MV3 runs.
 */
const ULID_ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

export function ulid(seedTime?: number): string {
  const t = seedTime ?? Date.now();
  let timeStr = "";
  let ts = t;
  for (let i = 9; i >= 0; i--) {
    timeStr = ULID_ALPHABET[ts % 32] + timeStr;
    ts = Math.floor(ts / 32);
  }
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  let randStr = "";
  for (let i = 0; i < 16; i++) {
    randStr += ULID_ALPHABET[bytes[i]! % 32];
  }
  return timeStr + randStr;
}
