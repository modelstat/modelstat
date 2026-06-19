/**
 * Self-register + claim flow.
 *
 * On first boot the SW calls POST /v1/devices/self-register with a
 * client-generated UUIDv7 + a browser fingerprint, and receives back
 * a device_secret (prefixed "ds_live_…"), claim_code, and claim_url.
 * The device is immediately usable — no user account required. All
 * ingest / heartbeat / discovery requests authenticate with the
 * device_secret as a Bearer token and land in a pending org that the
 * server auto-minted alongside the device.
 *
 * The user can optionally CLAIM the device by visiting the claim_url
 * (a public /d/:claim_code page) and signing in. On claim, the server
 * atomically migrates the pending org's data into the user's personal
 * org. The extension notices the flip by polling GET /v1/devices/me
 * and switches into "claimed" state reactively.
 *
 * Disconnect = clear local identity. Next boot silently re-registers
 * a brand-new device (fresh claim code, fresh pending org).
 */

import { DASHBOARD_URL, DEFAULT_API_URL, uuidv7 } from "@/common/config.js";
import { createLogger } from "@/common/logger.js";
import { getSetting, setSetting } from "@/storage/db.js";
import { postDiscovery } from "./discovery.js";
import { postHeartbeat } from "./heartbeat.js";

const log = createLogger("auth");

export type AuthStatus =
  | { kind: "registering" }
  | {
      kind: "unclaimed";
      deviceId: string;
      deviceUuid: string;
      claimCode: string;
      claimUrl: string;
      claimExpiresAt: string | null;
    }
  | {
      kind: "claimed";
      deviceId: string;
      deviceUuid: string;
      userId: string | null;
    }
  | { kind: "error"; message: string };

let currentStatus: AuthStatus = { kind: "registering" };

export function getAuthStatus(): AuthStatus {
  return currentStatus;
}

/** Returns the device_secret Bearer (prefixed `ds_live_…`) once the
 * device is registered. null during the first-boot registration race. */
export async function getBearerToken(): Promise<string | null> {
  return getSetting<string | null>("deviceSecret", null);
}

export async function getDeviceId(): Promise<string | null> {
  return getSetting<string | null>("deviceId", null);
}

export async function getDeviceUuid(): Promise<string | null> {
  return getSetting<string | null>("deviceUuid", null);
}

export async function getClaimCode(): Promise<string | null> {
  return getSetting<string | null>("claimCode", null);
}

export async function getClaimUrl(): Promise<string | null> {
  return getSetting<string | null>("claimUrl", null);
}

async function apiUrl(): Promise<string> {
  return (await getSetting<string>("apiUrl", DEFAULT_API_URL)) || DEFAULT_API_URL;
}

// ── Boot: load persisted identity and (if missing) self-register ─────

/** Called once on SW install/startup. Resolves the cached identity or
 * performs a fresh self-register. After that, one /devices/me poll is
 * done so the "claimed" flip is picked up even if the user claimed
 * while the SW was asleep. */
export async function bootAuth(): Promise<AuthStatus> {
  try {
    const deviceId = await getDeviceId();
    const secret = await getBearerToken();
    if (deviceId && secret) {
      // Existing identity — hydrate from storage then refresh from server.
      currentStatus = {
        kind: (await getSetting<string | null>("userId", null)) ? "claimed" : "unclaimed",
        deviceId,
        deviceUuid: (await getDeviceUuid()) ?? "",
        userId: await getSetting<string | null>("userId", null),
        claimCode: (await getClaimCode()) ?? "",
        claimUrl: (await getClaimUrl()) ?? "",
        claimExpiresAt: await getSetting<string | null>("claimExpiresAt", null),
      } as AuthStatus;
      await refreshSelf().catch((e) => log.warn("refreshSelf on boot failed", e));
    } else {
      currentStatus = { kind: "registering" };
      await selfRegister();
    }
  } catch (e) {
    currentStatus = { kind: "error", message: String((e as Error).message ?? e) };
    log.error("bootAuth failed", e);
  }
  // Fire initial heartbeat + discovery so the user sees the device
  // alive on /d/:claim_code within ~2 s.
  postDiscovery().catch((e) => log.warn("post-boot discovery failed", e));
  postHeartbeat().catch((e) => log.warn("post-boot heartbeat failed", e));
  return currentStatus;
}

async function selfRegister(): Promise<void> {
  const base = await apiUrl();
  const uuid = uuidv7();
  const body = {
    device_uuid: uuid,
    // public_key: omitted — the extension doesn't sign requests today.
    fingerprint: describeFingerprint(),
  };
  const res = await fetch(`${base}/v1/devices/self-register`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(`self-register ${res.status}: ${text.slice(0, 200)}`);
  }
  const reply = (await res.json()) as {
    device_uuid: string;
    device_id: string;
    device_secret: string;
    secret_prefix: string;
    claim_code: string;
    claim_url: string;
    status: "unclaimed";
    expires_at: string;
  };
  await setSetting("deviceUuid", reply.device_uuid);
  await setSetting("deviceId", reply.device_id);
  await setSetting("deviceSecret", reply.device_secret);
  await setSetting("claimCode", reply.claim_code);
  await setSetting("claimUrl", reply.claim_url);
  await setSetting("claimExpiresAt", reply.expires_at);
  await setSetting("userId", null);
  currentStatus = {
    kind: "unclaimed",
    deviceId: reply.device_id,
    deviceUuid: reply.device_uuid,
    claimCode: reply.claim_code,
    claimUrl: reply.claim_url,
    claimExpiresAt: reply.expires_at,
  };
  log.info(`self-registered ${reply.device_id} (claim ${reply.claim_code})`);
}

/** Refresh identity state from GET /v1/devices/me. Picks up the
 * claimed/unclaimed flip asynchronously (the user claims via the web,
 * the extension learns next time this runs). */
export async function refreshSelf(): Promise<AuthStatus> {
  const base = await apiUrl();
  const secret = await getBearerToken();
  if (!secret) return currentStatus;
  try {
    const res = await fetch(`${base}/v1/devices/me`, {
      headers: { authorization: `Bearer ${secret}` },
    });
    if (res.status === 401) {
      // Our secret is no longer valid (device wiped server-side, etc.).
      // Re-register cleanly.
      log.warn("device_secret rejected — re-registering");
      await wipeIdentity();
      await selfRegister();
      return currentStatus;
    }
    if (!res.ok) {
      log.warn(`/devices/me ${res.status}`);
      return currentStatus;
    }
    const me = (await res.json()) as {
      device_id: string;
      device_uuid: string;
      status: "unclaimed" | "claimed";
      claimed_at: string | null;
      claim_code: string | null;
      claim_url: string | null;
      claim_expires_at: string | null;
      user_id: string | null;
    };
    if (me.status === "claimed") {
      await setSetting("userId", me.user_id);
      // claim_code/claim_url go null on the server once claimed;
      // clear locally so the popup hides the claim banner.
      await setSetting("claimCode", null);
      await setSetting("claimUrl", null);
      await setSetting("claimExpiresAt", null);
      currentStatus = {
        kind: "claimed",
        deviceId: me.device_id,
        deviceUuid: me.device_uuid,
        userId: me.user_id,
      };
    } else {
      await setSetting("claimCode", me.claim_code);
      await setSetting("claimUrl", me.claim_url);
      await setSetting("claimExpiresAt", me.claim_expires_at);
      await setSetting("userId", null);
      currentStatus = {
        kind: "unclaimed",
        deviceId: me.device_id,
        deviceUuid: me.device_uuid,
        claimCode: me.claim_code ?? "",
        claimUrl: me.claim_url ?? "",
        claimExpiresAt: me.claim_expires_at,
      };
    }
  } catch (e) {
    log.warn("/devices/me threw", e);
  }
  return currentStatus;
}

/** Open the claim_url in a new tab — links from popup. */
export function openClaim(): void {
  (async () => {
    const claimUrl = await getClaimUrl();
    if (claimUrl) {
      chrome.tabs.create({ url: claimUrl });
      return;
    }
    // Fallback: refresh self and retry once.
    await refreshSelf();
    const u = await getClaimUrl();
    if (u) chrome.tabs.create({ url: u });
    else chrome.tabs.create({ url: `${DASHBOARD_URL}/dashboard/devices` });
  })();
}

/** Wipe identity + force a fresh self-register on next boot. Exposed
 * from Options as "disconnect" — any data this device produced before
 * it was claimed stays addressable via the old claim_code (server
 * keeps the row). A fresh unclaimed device is minted; the user sees
 * it appear at /dashboard/devices and can claim it separately. */
export async function disconnect(): Promise<void> {
  await wipeIdentity();
  currentStatus = { kind: "registering" };
  await selfRegister().catch((e) => {
    currentStatus = { kind: "error", message: String((e as Error).message ?? e) };
  });
  postDiscovery().catch(() => {});
  postHeartbeat().catch(() => {});
}

async function wipeIdentity(): Promise<void> {
  for (const k of [
    "deviceId",
    "deviceUuid",
    "deviceSecret",
    "claimCode",
    "claimUrl",
    "claimExpiresAt",
    "userId",
  ]) {
    await setSetting(k, null);
  }
}

function describeFingerprint(): Record<string, unknown> {
  const ua = navigator.userAgent;
  const osFamily = /Mac/.test(ua)
    ? "macos"
    : /Windows/.test(ua)
      ? "windows"
      : /Linux|X11|CrOS/.test(ua)
        ? "linux"
        : "other";
  const browser = /Edg\//.test(ua)
    ? "Edge"
    : /OPR\//.test(ua)
      ? "Opera"
      : /Brave/.test(ua)
        ? "Brave"
        : /Arc/.test(ua)
          ? "Arc"
          : /Chrome\//.test(ua)
            ? "Chrome"
            : "Chromium";
  const osLabel = /Mac/.test(ua) ? "macOS" : /Windows/.test(ua) ? "Windows" : /Linux|X11/.test(ua) ? "Linux" : "";
  return {
    hostname: osLabel ? `${browser} on ${osLabel}` : browser,
    os_family: osFamily,
    // DeviceEnrollment schema caps os_version at 60 chars. Extract a
    // short marker (e.g. "Mac OS X 10_15_7") rather than pasting the
    // full User-Agent.
    os_version: extractOsVersion(ua),
    arch: "other",
    daemon_version: `modelstat-extension@${chrome.runtime.getManifest().version}`,
    surface: "chrome_extension",
    browser,
  };
}

function extractOsVersion(ua: string): string {
  const mMac = /Mac OS X ([\d_.]+)/.exec(ua);
  if (mMac) return `macOS ${mMac[1]!.replace(/_/g, ".")}`.slice(0, 60);
  const mWin = /Windows NT ([\d.]+)/.exec(ua);
  if (mWin) return `Windows NT ${mWin[1]}`.slice(0, 60);
  const mLinux = /X11; ([^)]+)\)/.exec(ua);
  if (mLinux) return `Linux ${mLinux[1]}`.slice(0, 60);
  return ua.slice(0, 60);
}
