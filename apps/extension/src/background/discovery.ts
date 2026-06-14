/**
 * Discovery reporting — tell the modelstat API which web-chat providers
 * this extension is tracking, so they appear under the user's device
 * on the Devices dashboard.
 *
 * We run this once right after auth connects, then daily via an alarm.
 * Unlike CLI agents that scan the filesystem, the Chrome extension's
 * "installations" are determined by the four adapter configs it has
 * loaded — there's no local fs to walk. So the payload is simple:
 * one installation per bundled adapter, with install_method set to
 * "chrome_extension" and detected_via listing the adapter source.
 */

import { DEFAULT_API_URL } from "@/common/config.js";
import { createLogger } from "@/common/logger.js";
import { allAdapters } from "./adapter-registry.js";
import { getBearerToken, getDeviceId } from "./auth.js";
import { getSetting } from "@/storage/db.js";

const log = createLogger("discovery");

export async function postDiscovery(): Promise<void> {
  const token = await getBearerToken();
  const deviceId = await getDeviceId();
  if (!token || !deviceId) return; // not connected yet; nothing to report

  const apiUrl = (await getSetting<string>("apiUrl", DEFAULT_API_URL)) || DEFAULT_API_URL;
  const version = chrome.runtime.getManifest().version;

  const installations = Object.values(allAdapters()).map((adapter) => ({
    agent: adapter.provider,
    install_method: "chrome_extension" as const,
    binary_path: null,
    data_dir: null,
    version: `adapter@${adapter.adapter_version}`,
    detected_via: ["chrome_extension", `modelstat_adapter_${adapter.adapter_version}`],
  }));

  if (installations.length === 0) {
    log.debug("no adapters loaded yet — skipping discovery");
    return;
  }

  const body = {
    device_id: deviceId,
    installations,
    identities: [], // v1: no identity scraping from chat sites
    scanned_at: new Date().toISOString(),
    companion_version: `modelstat-extension@${version}`,
  };

  try {
    const res = await fetch(`${apiUrl}/v1/devices/discovery`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify(body),
    });
    if (!res.ok) {
      log.warn(`discovery ${res.status}`);
      return;
    }
    const reply = (await res.json()) as { installations_upserted: number };
    log.info(`reported ${reply.installations_upserted} installations`);
  } catch (e) {
    log.warn("discovery failed", e);
  }
}

export function setupDiscoveryAlarm(): void {
  chrome.alarms.create("modelstat-discovery", { periodInMinutes: 24 * 60 });
}
