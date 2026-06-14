/**
 * Periodic heartbeat to /v1/agent/heartbeat. Same endpoint the CLI
 * agent uses — bumps `agentLastHeartbeatAt` so:
 *   - the /connect page can flip from "approved" → "heartbeat received"
 *     and redirect the user to /dashboard/devices
 *   - the Devices page dot shows online/stale/offline correctly
 *
 * Fires immediately on connect and again every 60 s.
 */

import { DEFAULT_API_URL, AGENT_VERSION } from "@/common/config.js";
import { createLogger } from "@/common/logger.js";
import { db, getSetting } from "@/storage/db.js";
import { getBearerToken, getDeviceId } from "./auth.js";
import { bump } from "./counters.js";

const log = createLogger("heartbeat");

async function attempt(token: string, deviceId: string, apiUrl: string): Promise<{ ok: true } | { ok: false; status?: number; error: string }> {
  const queueSize = await db().events.where("synced").equals(0 as unknown as number).count();
  // Default ON, matching ingest-queue + the background settings reader — a
  // fresh, claimed device reports "watching" while it's actually syncing,
  // not "idle".
  const syncEnabled = await getSetting<boolean>("syncEnabled", true);
  try {
    const res = await fetch(`${apiUrl}/v1/agent/heartbeat`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({
        device_id: deviceId,
        status: syncEnabled ? "watching" : "idle",
        message: null,
        progress_done: 0,
        progress_total: 0,
        queue_size: queueSize,
        stats: {},
        last_event_at: null,
        companion_version: AGENT_VERSION,
      }),
    });
    if (res.ok) return { ok: true };
    const body = await res.text().catch(() => "");
    return { ok: false, status: res.status, error: body || `status ${res.status}` };
  } catch (e) {
    return { ok: false, error: String((e as Error).message ?? e) };
  }
}

export async function postHeartbeat(): Promise<void> {
  const token = await getBearerToken();
  const deviceId = await getDeviceId();
  if (!token || !deviceId) {
    log.debug("skip heartbeat: no token/deviceId yet");
    return;
  }
  const apiUrl = (await getSetting<string>("apiUrl", DEFAULT_API_URL)) || DEFAULT_API_URL;

  // Retry up to 3 times with short backoff. Common transient cause: the
  // register→heartbeat race where the bearer row on the server hasn't
  // quite seen the deviceId link yet.
  for (let i = 0; i < 3; i++) {
    const r = await attempt(token, deviceId, apiUrl);
    if (r.ok) {
      bump("heartbeats");
      log.info(`heartbeat ok (attempt ${i + 1})`);
      return;
    }
    log.warn(`heartbeat attempt ${i + 1} failed (${r.status ?? "-"}): ${r.error}`);
    if (r.status === 401 || r.status === 403) {
      // Auth-level failure — retry won't help.
      return;
    }
    await new Promise((r) => setTimeout(r, 500 * (i + 1)));
  }
  log.error("heartbeat failed after 3 attempts");
}

export function setupHeartbeatAlarm(): void {
  chrome.alarms.create("modelstat-heartbeat", { periodInMinutes: 1 });
}
