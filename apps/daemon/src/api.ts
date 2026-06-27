import { request } from "undici";
import type { DiscoveryReport, IngestBatch } from "@modelstat/core";
import { IngestClient } from "@modelstat/daemon-core/http";
import { createLogger } from "@modelstat/daemon-core/logger";
import { state } from "./config.js";

/* ─── Phase 2: self-register / claim ──────────────────────────────── */

export type SelfRegisterResponse = {
  device_uuid: string;
  device_id: string;
  device_secret: string;
  secret_prefix: string;
  claim_code: string;
  claim_url: string;
  status: "unclaimed";
  expires_at: string;
};

export type DeviceMeResponse = {
  device_id: string;
  device_uuid: string | null;
  self_registered: boolean;
  status: "unclaimed" | "claimed";
  claimed_at: string | null;
  claim_code: string | null;
  claim_url: string | null;
  claim_expires_at: string | null;
  user_id: string | null;
  secret_prefix: string | null;
  hostname: string | null;
  daemon_status: string | null;
  last_seen_at: string | null;
  fingerprint: Record<string, unknown> | null;
};

export async function selfRegister(input: {
  device_uuid: string;
  public_key?: string;
  fingerprint: Record<string, string | number | boolean>;
}): Promise<SelfRegisterResponse> {
  const res = await request(`${state.apiUrl}/v1/devices/self-register`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(input),
  });
  if (res.statusCode >= 300) {
    throw new Error(`self-register failed: ${res.statusCode} ${await res.body.text()}`);
  }
  return (await res.body.json()) as SelfRegisterResponse;
}

export class DeviceMeUnauthorized extends Error {
  constructor() {
    super("devices/me returned 401: device_secret not recognised");
    this.name = "DeviceMeUnauthorized";
  }
}

export async function fetchDeviceMe(secret: string): Promise<DeviceMeResponse> {
  const res = await request(`${state.apiUrl}/v1/devices/me`, {
    method: "GET",
    headers: { authorization: `Bearer ${secret}` },
  });
  if (res.statusCode === 401) {
    // Discard body — callers only need the sentinel. Bearer either
    // expired, was rotated, or belongs to a device row that got
    // deleted server-side. Either way the only recovery is a fresh
    // self-register.
    await res.body.dump();
    throw new DeviceMeUnauthorized();
  }
  if (res.statusCode >= 300) {
    throw new Error(`devices/me failed: ${res.statusCode} ${await res.body.text()}`);
  }
  return (await res.body.json()) as DeviceMeResponse;
}

export async function rotateDeviceSecret(currentSecret: string): Promise<{
  device_id: string;
  device_secret: string;
  secret_prefix: string;
  rotated_at: string;
}> {
  const res = await request(`${state.apiUrl}/v1/devices/me/rotate-secret`, {
    method: "POST",
    headers: { authorization: `Bearer ${currentSecret}` },
  });
  if (res.statusCode >= 300) {
    throw new Error(`rotate-secret failed: ${res.statusCode} ${await res.body.text()}`);
  }
  return (await res.body.json()) as {
    device_id: string;
    device_secret: string;
    secret_prefix: string;
    rotated_at: string;
  };
}

export async function reportDiscovery(report: DiscoveryReport): Promise<void> {
  const bearer = state.bearer;
  if (!bearer) throw new Error("daemon not enrolled — run `register` first");
  const res = await request(`${state.apiUrl}/v1/devices/discovery`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${bearer}` },
    body: JSON.stringify(report),
  });
  if (res.statusCode >= 300) {
    const body = await res.body.text();
    throw new Error(`reportDiscovery failed: ${res.statusCode} ${body}`);
  }
}

/* ─── Defensive authenticated JSON GET ────────────────────────────────
 * The server is a single origin that serves both the JSON API and the SPA: a
 * removed/renamed route falls through to the SPA fallback and returns 200 +
 * `text/html`, so a naive `res.body.json()` throws "Unexpected token <". This
 * helper makes every read DEGRADE instead of crash — a `text/html` content-type
 * (the SPA fallback) OR a JSON-parse failure ⇒ the endpoint is gone ⇒ `null`.
 * Bearer-authed; `null` on no-bearer / non-2xx / SPA-fallback / parse failure.
 */
async function authedJsonGet<T>(url: string): Promise<T | null> {
  const bearer = state.bearer;
  if (!bearer) return null;
  let res: Awaited<ReturnType<typeof request>>;
  try {
    res = await request(url, { method: "GET", headers: { authorization: `Bearer ${bearer}` } });
  } catch {
    return null; // network blip — caller treats as "unavailable", retries later
  }
  if (res.statusCode >= 300) {
    await res.body.dump();
    return null;
  }
  // The SPA fallback returns 200 + text/html for a route the server no longer
  // exposes. Detect it BEFORE parsing so a future route removal degrades to null
  // rather than throwing "Unexpected token <" deep in a caller.
  const ctype = (res.headers["content-type"] ?? "").toString().toLowerCase();
  if (ctype.includes("text/html")) {
    await res.body.dump();
    return null;
  }
  try {
    return (await res.body.json()) as T;
  } catch {
    // Non-JSON body despite a non-html content-type (or a truncated read) — the
    // route is effectively gone from this caller's perspective.
    return null;
  }
}

// Shared IngestClient wired once and reused for the process lifetime.
// Retry matrix (400=drop, 401=reauth, 429/5xx=backoff) lives in
// @modelstat/daemon-core/http — CLI just passes its AuthProvider.
let _ingest: IngestClient | null = null;

function ingestClient(): IngestClient {
  if (_ingest) return _ingest;
  const logger = createLogger("daemon.ingest");
  _ingest = new IngestClient({
    apiUrl: state.apiUrl,
    auth: {
      getToken: async () => state.bearer,
      onInvalidToken: async () => {
        // On 401/403, attempt a secret rotation; if that succeeds the
        // new bearer is persisted to conf and getToken() picks it up.
        const current = state.bearer;
        if (!current) return false;
        try {
          await rotateDeviceSecret(current);
          return !!state.bearer;
        } catch {
          return false;
        }
      },
    },
    logger,
  });
  return _ingest;
}

export type UploadOutcome =
  | {
      committed: true;
      accepted: number;
      new_sessions: number;
      updated_sessions: number;
      batch_id: string;
    }
  // The server PERMANENTLY rejected this batch (400/422 — malformed). The caller
  // must QUARANTINE it (skip past + alert), not retry — see scan.ts.
  | { committed: false; reason: string };

export async function uploadBatch(batch: IngestBatch): Promise<UploadOutcome> {
  const result = await ingestClient().upload(batch);
  if (result.kind === "commit") {
    return {
      committed: true,
      accepted: result.response.accepted,
      new_sessions: result.response.new_sessions,
      updated_sessions: result.response.updated_sessions,
      batch_id: result.response.batch_id,
    };
  }
  // A PERMANENT reject is returned (not thrown) so the scan loop can advance past
  // this one poison batch and keep newer data flowing. A TRANSIENT failure (no
  // token / reauth / 5xx-exhausted / network) still throws → the batch is held
  // and retried next cycle, never dropped on a server/network blip.
  if (result.permanent) {
    return { committed: false, reason: result.reason };
  }
  throw new Error(`upload failed: ${result.reason}`);
}

/* ─── Self-healing reconcile (anti-entropy) ───────────────────────────
 * The server is authoritative for what's ingested. The daemon fetches the
 * server's per-session event digest for its scope and re-ships any session the
 * server is short on (see reconcile.ts). Read-only; device-secret auth. */

/** Top level: scope-wide total (the O(1) "anything to do?" check) + per-day rollup. */
export interface BackfillDays {
  total_events: number;
  days: Array<{ day: string; events: number }>;
}
/** Drill level: one day's per-session counts (fetched only for divergent days). */
export interface BackfillDaySessions {
  day: string;
  sessions: Array<{ session_id: string; events: number }>;
}

function backfillGet<T>(query: string): Promise<T | null> {
  // Routed through the defensive GET so a future removal of /v1/backfill/digests
  // degrades reconcile to a no-op (null ⇒ "skip this pass") instead of crashing
  // on the SPA-fallback HTML.
  return authedJsonGet<T>(`${state.apiUrl}/v1/backfill/digests${query}`);
}

/** Per-day digest for this device's scope (top of the reconcile tree). */
export function fetchBackfillDays(): Promise<BackfillDays | null> {
  return backfillGet<BackfillDays>("");
}

/** One day's per-session counts — fetched only for days whose total diverged. */
export function fetchBackfillDaySessions(day: string): Promise<BackfillDaySessions | null> {
  return backfillGet<BackfillDaySessions>(`?day=${encodeURIComponent(day)}`);
}

/* ─── Loopback control plane ──────────────────────────────────────────
 * `modelstat sync --session` first asks a RUNNING daemon (warm summariser) to
 * force-scan, via the loopback control endpoint the receiver serves. Only when
 * no daemon is listening (ECONNREFUSED) does the CLI fall back to a cold
 * in-process scan. */

/** Loopback control-plane port — the same 127.0.0.1 server the SDK ingest
 * uses (resolution mirrors receiver.ts: env override, else 4319). Resolved
 * here to avoid an api↔receiver import cycle. */
function controlPort(): number {
  return Number(process.env.MODELSTAT_LOCAL_INGEST_PORT) || 4319;
}

export type ControlScanOutcome =
  /** A running daemon accepted (and, with `wait`, finished) the scan. */
  | { kind: "ok"; started: boolean; scanned: boolean }
  /** Nothing is listening on the loopback control port — no daemon running. */
  | { kind: "no_daemon" }
  /** The daemon responded with an error (bad target, scan failure, …). */
  | { kind: "error"; status: number; message: string };

/**
 * POST the loopback control endpoint to force-scan a session on a running
 * daemon. Resolves `no_daemon` on connection-refused so the caller can fall
 * back to a standalone scan; surfaces other failures as `error`.
 */
export async function postControlScan(
  body: { session_ids?: string[]; file?: string; wait?: boolean },
  opts: { port?: number } = {},
): Promise<ControlScanOutcome> {
  const port = opts.port ?? controlPort();
  try {
    const res = await request(`http://127.0.0.1:${port}/v1/control/scan`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
      // A `wait:true` scan can take a while (cold-ish summariser, big session);
      // give it room but don't hang forever. undici's headersTimeout/bodyTimeout
      // default to 30s — lift them for the waiting case.
      headersTimeout: body.wait ? 600_000 : 5_000,
      bodyTimeout: body.wait ? 600_000 : 5_000,
    });
    if (res.statusCode >= 300) {
      const message = await res.body.text().catch(() => "");
      return { kind: "error", status: res.statusCode, message };
    }
    const data = (await res.body.json().catch(() => ({}))) as {
      started?: boolean;
      scanned?: boolean;
    };
    return { kind: "ok", started: data.started === true, scanned: data.scanned === true };
  } catch (e) {
    const code = (e as NodeJS.ErrnoException).code;
    if (code === "ECONNREFUSED" || code === "ECONNRESET") return { kind: "no_daemon" };
    return { kind: "error", status: 0, message: (e as Error).message };
  }
}
