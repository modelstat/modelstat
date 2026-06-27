import { request } from "undici";
import type { IngestBatch } from "@modelstat/core";
import { expBackoff } from "@modelstat/daemon-core/config";
import { IngestClient } from "@modelstat/daemon-core/http";
import { createLogger } from "@modelstat/daemon-core/logger";
import { state } from "./config.js";
import { buildFingerprint, intendedDeviceUuid } from "./machine-key.js";

/* ─── Server envelope ──────────────────────────────────────────────────
 * Every JSON API response wraps its payload in `{ data: … }`. The daemon
 * unwraps `.data` for the device endpoints (tokens / heartbeat / devices-me /
 * rotate). Ingest keeps its own bare shape (see daemon-core/http). */
function unwrapData<T>(body: unknown): T {
  if (body && typeof body === "object" && "data" in (body as Record<string, unknown>)) {
    return (body as { data: T }).data;
  }
  // No envelope — accept the raw body so a server that returns the payload
  // un-wrapped (or a test stub) still parses rather than crashing.
  return body as T;
}

/* ─── Register (the ONLY register door): POST /v1/tokens ────────────────── */

export type SelfRegisterResponse = {
  device_id: string;
  device_uuid: string;
  /** Opaque device secret — `ds_live_<64-hex>`. The daemon STORES it verbatim
   * and sends it as `Authorization: Bearer <secret>`; it makes no client-side
   * assumptions about the format. */
  device_secret: string;
  secret_prefix: string;
  /** null once the device is already claimed (the server returns no fresh
   * claim handle for a claimed row). */
  claim_code: string | null;
  claim_url: string | null;
  status: "unclaimed" | "claimed";
  user_id: string | null;
  /** Set when the server reused an existing row (same machine_id) and minted a
   * fresh secret instead of creating a new device. */
  re_registered?: boolean;
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
  /** Last time the server saw this device (heartbeat / ingest). */
  last_active_at: string | null;
  fingerprint: Record<string, unknown> | null;
};

/**
 * Register this device against the server's ONE register door,
 * `POST /v1/tokens`. The `fingerprint.machine_id` is the dedupe anchor: the
 * server reuses the existing row for a machine it has already seen and returns
 * a fresh `ds_live_` secret (with `re_registered: true`), so re-registering is
 * convergent — it never orphans a claimed device into a duplicate.
 */
export async function selfRegister(input: {
  device_uuid: string;
  public_key?: string;
  fingerprint: Record<string, string | number | boolean>;
}): Promise<SelfRegisterResponse> {
  const res = await request(`${state.apiUrl}/v1/tokens`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(input),
  });
  if (res.statusCode >= 300) {
    throw new Error(`register failed: ${res.statusCode} ${await res.body.text()}`);
  }
  return unwrapData<SelfRegisterResponse>(await res.body.json());
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
    // register (machine-stable — see recoverIdentity).
    await res.body.dump();
    throw new DeviceMeUnauthorized();
  }
  if (res.statusCode >= 300) {
    throw new Error(`devices/me failed: ${res.statusCode} ${await res.body.text()}`);
  }
  return unwrapData<DeviceMeResponse>(await res.body.json());
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
  return unwrapData<{
    device_id: string;
    device_secret: string;
    secret_prefix: string;
    rotated_at: string;
  }>(await res.body.json());
}

/* ─── Machine-stable identity recovery ─────────────────────────────────────
 * The single recovery routine for "the server no longer accepts our bearer"
 * (401/403). Rotate-secret CANNOT recover here: a revoked/deleted row has no
 * valid current secret to authenticate the rotate with. The only door that
 * works is re-register — and because we re-register with the SAME
 * deterministic device_uuid + the SAME `fingerprint.machine_id`, the server
 * reuses the existing row and hands back a fresh `ds_live_` secret. So
 * recovery lands on the same device, never a duplicate.
 *
 * Exponential backoff (module-level so it persists across the per-second
 * call sites) protects a TRULY-deleted row — one the server refuses to
 * re-create — from hot-looping register calls.
 */
let recoverBackoffUntil = 0;
let recoverAttempt = 0;
const RECOVER_BACKOFF_MS = [0, 2_000, 5_000, 15_000, 30_000, 60_000] as const;

export async function recoverIdentity(): Promise<boolean> {
  const now = Date.now();
  if (now < recoverBackoffUntil) return false; // still cooling down from a recent failure
  try {
    // Drop the in-memory bearer first. This also resets `state.deviceUuid` to
    // null, so selfRegister re-derives the machine-stable UUID rather than
    // reusing whatever stale value was cached.
    state.setBearer(null);
    const res = await selfRegister({
      device_uuid: intendedDeviceUuid(),
      fingerprint: buildFingerprint(),
    });
    // selfRegister only returns the payload; persist it the same way
    // cmdSelfRegister does so the fresh secret + device_id are durable.
    state.saveFreshIdentity({
      deviceUuid: res.device_uuid,
      deviceId: res.device_id,
      bearerToken: res.device_secret,
      claimCode: res.claim_code,
      claimUrl: res.claim_url,
    });
    // Success — reset the backoff so the next genuine 401 recovers immediately.
    recoverAttempt = 0;
    recoverBackoffUntil = 0;
    return !!state.bearer;
  } catch {
    const delay = RECOVER_BACKOFF_MS[Math.min(recoverAttempt, RECOVER_BACKOFF_MS.length - 1)]!;
    recoverAttempt++;
    recoverBackoffUntil = Date.now() + delay;
    return false;
  }
}

/* ─── Shared authenticated device-API client ──────────────────────────────
 * ONE client for the daemon's authenticated device calls (heartbeat /
 * devices/me / rotate / backfill) so the auth + 401-recovery + 5xx-backoff
 * matrix is identical everywhere — the same matrix the IngestClient uses for
 * /v1/ingest:
 *   2xx       → return parsed body
 *   400/422   → drop (return null; permanent client error)
 *   401/403   → recoverIdentity() once, retry with the fresh bearer
 *   408/426/429/5xx → exponential backoff + retry (HOLD, never drop)
 *   text/html (SPA fallback for a removed route) → null (degrade, don't crash)
 * GETs degrade to `null`; POSTs return the parsed body or `null` (caller
 * decides whether `null` is fatal). Mirrors daemon-core/http so no daemon
 * device call eats every non-2xx the way the old hand-rolled heartbeat did.
 */

const deviceLogger = createLogger("daemon.device");

/** The SPA fallback returns 200 + text/html for a route the server no longer
 * exposes — detect it BEFORE parsing so a route removal degrades to null
 * instead of throwing "Unexpected token <". */
function isSpaFallback(res: Awaited<ReturnType<typeof request>>): boolean {
  const ctype = (res.headers["content-type"] ?? "").toString().toLowerCase();
  return ctype.includes("text/html");
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

type DeviceRequest = {
  method: "GET" | "POST";
  url: string;
  /** JSON body for POSTs. */
  body?: unknown;
  /** Max attempts (covers reauth + backoff retries). Default 3. */
  maxAttempts?: number;
};

/**
 * Single authed request driver. Returns the parsed JSON body (envelope
 * unwrapped via the caller, NOT here) on 2xx, or `null` for any non-recoverable
 * outcome (no bearer, 4xx-permanent, SPA fallback, parse failure, or
 * attempts-exhausted on 5xx/network). 401/403 triggers exactly one
 * recoverIdentity() per attempt cycle, then retries with the new bearer.
 */
async function deviceRequest(req: DeviceRequest): Promise<unknown | null> {
  const maxAttempts = req.maxAttempts ?? 3;
  for (let attempt = 0; attempt < maxAttempts; attempt++) {
    const bearer = state.bearer;
    if (!bearer) {
      // No token at all — try a one-shot recovery, then retry. If recovery is
      // backing off (or fails), give up for this call.
      const recovered = await recoverIdentity();
      if (!recovered) return null;
      continue;
    }
    let res: Awaited<ReturnType<typeof request>>;
    try {
      res = await request(req.url, {
        method: req.method,
        headers: {
          authorization: `Bearer ${bearer}`,
          ...(req.body !== undefined ? { "content-type": "application/json" } : {}),
        },
        ...(req.body !== undefined ? { body: JSON.stringify(req.body) } : {}),
      });
    } catch (err) {
      // Network blip — back off and retry; never drop on a transient failure.
      deviceLogger.warn("device request failed", {
        url: req.url,
        attempt,
        err: (err as Error).message,
      });
      await sleep(expBackoff(attempt));
      continue;
    }
    if (res.statusCode >= 200 && res.statusCode < 300) {
      if (isSpaFallback(res)) {
        // 200 + HTML = the route is gone (SPA fallback). Degrade to null.
        await res.body.dump();
        return null;
      }
      try {
        return await res.body.json();
      } catch {
        await res.body.dump().catch(() => undefined);
        return null;
      }
    }
    if (res.statusCode === 401 || res.statusCode === 403) {
      await res.body.dump();
      // The bearer was revoked/rotated/deleted server-side. Rotate-secret can't
      // help post-revocation — recover by machine-stable re-register.
      const recovered = await recoverIdentity();
      if (!recovered) return null;
      continue; // retry with the fresh bearer
    }
    if (res.statusCode === 400 || res.statusCode === 422) {
      // Permanent client error — the request will never succeed as-is.
      const text = await res.body.text().catch(() => "");
      deviceLogger.error("device request rejected", {
        url: req.url,
        status: res.statusCode,
        body: text.slice(0, 300),
      });
      return null;
    }
    if (
      res.statusCode === 408 ||
      res.statusCode === 426 ||
      res.statusCode === 429 ||
      res.statusCode >= 500
    ) {
      await res.body.dump();
      await sleep(expBackoff(attempt));
      continue; // transient — HOLD + retry
    }
    // Any other 4xx — treat as permanent.
    await res.body.dump();
    return null;
  }
  return null; // attempts exhausted
}

/**
 * Defensive authenticated JSON GET — bearer-authed read that DEGRADES to null
 * (no-bearer / non-2xx / SPA fallback / parse failure / attempts-exhausted)
 * rather than crashing a caller, and recovers identity on 401. Used by the
 * backfill/reconcile reads. `T` is the raw JSON shape (no envelope unwrap —
 * backfill responses are top-level).
 */
async function authedJsonGet<T>(url: string): Promise<T | null> {
  return (await deviceRequest({ method: "GET", url })) as T | null;
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
      // On 401/403, recover the identity by machine-stable re-register. Rotating
      // the secret CANNOT recover here — a revoked/deleted row has no valid
      // current secret to rotate with — which is why ingest reauth no longer
      // routes through rotateDeviceSecret. recoverIdentity persists the fresh
      // bearer so getToken() picks it up on the retry.
      onInvalidToken: async () => recoverIdentity(),
    },
    logger,
  });
  return _ingest;
}

/* ─── Authenticated heartbeat POST ────────────────────────────────────────
 * Liveness (folds discovery). Routed through the shared device client so the
 * heartbeat gets the SAME 401→recover + 5xx-backoff handling as every other
 * device call — replacing the old hand-rolled POST that swallowed every
 * non-2xx. Returns the unwrapped `.data` on success, or null on failure. */
export type HeartbeatResponse = {
  commands?: unknown;
  server_time?: string;
  daemon_release?: { verdict?: string; min?: string | null; latest?: string | null };
  installations_upserted?: number;
  identities_upserted?: number;
};

export async function postHeartbeat(
  deviceId: string,
  body: Record<string, unknown>,
): Promise<HeartbeatResponse | null> {
  const raw = await deviceRequest({
    method: "POST",
    url: `${state.apiUrl}/v1/devices/${encodeURIComponent(deviceId)}/heartbeat`,
    body,
  });
  if (raw == null) return null;
  return unwrapData<HeartbeatResponse>(raw);
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
