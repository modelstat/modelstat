import { request } from "undici";
import type { DiscoveryReport, IngestBatch } from "@modelstat/core";
import { IngestClient } from "@modelstat/companion-core/http";
import { createLogger } from "@modelstat/companion-core/logger";
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
  companion_status: string | null;
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

/* ─── Claim-code capability endpoints ─────────────────────────────
 * The /v1/device/:claim_code/* surface is authorised by the claim_code
 * itself — no bearer required. These helpers let the local CLI pull
 * its own stats without a second round-trip through auth. Once the
 * device is claimed, the endpoint 404s and callers fall back to a
 * "see the dashboard at …" message.
 */

export interface DeviceViewSummary {
  claim_code: string;
  status: "claimed" | "unclaimed";
  device: {
    id: string;
    hostname: string | null;
    os_family: string | null;
    companion_version: string | null;
    companion_status: string | null;
    last_seen_at: string | null;
  };
  analyzed: { count: number; totalTokens: string; totalCostUsd: number };
  free_quota_tokens: string;
  claim_url: string;
  agent_url: string;
}

export async function fetchDeviceViewByClaim(
  claimCode: string,
): Promise<DeviceViewSummary | null> {
  const res = await request(`${state.apiUrl}/v1/device/${encodeURIComponent(claimCode)}`, {
    method: "GET",
  });
  if (res.statusCode === 404) return null;
  if (res.statusCode >= 300) {
    throw new Error(`device-view failed: ${res.statusCode} ${await res.body.text()}`);
  }
  return (await res.body.json()) as DeviceViewSummary;
}

export interface DeviceViewJobs {
  pending: number;
  running: number;
}

export async function fetchDeviceViewJobsByClaim(
  claimCode: string,
): Promise<DeviceViewJobs | null> {
  const res = await request(
    `${state.apiUrl}/v1/device/${encodeURIComponent(claimCode)}/jobs`,
    { method: "GET" },
  );
  if (res.statusCode === 404) return null;
  if (res.statusCode >= 300) {
    throw new Error(`device-view jobs failed: ${res.statusCode} ${await res.body.text()}`);
  }
  return (await res.body.json()) as DeviceViewJobs;
}

export interface DeviceViewLedgerRow {
  id: string;
  occurred_at: string;
  kind: string;
  session_id: string | null;
  tokens: string;
}

export async function fetchDeviceViewLedgerByClaim(
  claimCode: string,
): Promise<{ recent: DeviceViewLedgerRow[] } | null> {
  const res = await request(
    `${state.apiUrl}/v1/device/${encodeURIComponent(claimCode)}/ledger`,
    { method: "GET" },
  );
  if (res.statusCode === 404) return null;
  if (res.statusCode >= 300) {
    throw new Error(`device-view ledger failed: ${res.statusCode} ${await res.body.text()}`);
  }
  return (await res.body.json()) as { recent: DeviceViewLedgerRow[] };
}

// Shared IngestClient wired once and reused for the process lifetime.
// Retry matrix (400=drop, 401=reauth, 429/5xx=backoff) lives in
// @modelstat/companion-core/http — CLI just passes its AuthProvider.
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

export async function uploadBatch(batch: IngestBatch): Promise<{
  accepted: number;
  new_sessions: number;
  updated_sessions: number;
  batch_id: string;
}> {
  const result = await ingestClient().upload(batch);
  if (result.kind !== "commit") {
    throw new Error(`upload failed: ${result.reason}`);
  }
  return {
    accepted: result.response.accepted,
    new_sessions: result.response.new_sessions,
    updated_sessions: result.response.updated_sessions,
    batch_id: result.response.batch_id,
  };
}
