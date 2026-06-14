/**
 * Adapter registry.
 *
 *   1. On boot, load bundled adapters from extension resources (never
 *      fails — the extension must work offline).
 *   2. On a 15-min alarm, fetch the signed manifest from the API. For
 *      each entry whose version is newer than what we have, fetch the
 *      signed config, verify, and swap it in (per-provider atomic).
 *   3. Broadcast `adapter-updated` to active content scripts so they
 *      re-install DOM observers.
 *   4. On invariant failure, push a breakage row (drained by the
 *      telemetry task).
 */

import type { AdapterConfig } from "@modelstat/adapters-protocol";
import { AdapterConfig as AdapterConfigSchema } from "@modelstat/adapters-protocol";
import { ADAPTER_POLL_INTERVAL_MS, DEFAULT_API_URL } from "@/common/config.js";
import { createLogger } from "@/common/logger.js";
import { verifySignedAdapter } from "@/interpreter/signature.js";
import { db, getSetting } from "@/storage/db.js";

const log = createLogger("adapters");

const BUNDLED_FILES: Record<string, string> = {
  chatgpt_web: "adapters/chatgpt_web.json",
  claude_web: "adapters/claude_web.json",
  gemini_web: "adapters/gemini_web.json",
  grok_web: "adapters/grok_web.json",
};

const state = new Map<string, AdapterConfig>();

export async function initAdapters(): Promise<void> {
  for (const [provider, path] of Object.entries(BUNDLED_FILES)) {
    try {
      const res = await fetch(chrome.runtime.getURL(path));
      const raw = await res.json();
      const parsed = AdapterConfigSchema.safeParse(raw);
      if (parsed.success) state.set(provider, parsed.data);
      else log.error(`bundled ${provider} invalid`, parsed.error.issues);
    } catch (e) {
      log.error(`bundled ${provider} load failed`, e);
    }
  }
  log.info(`loaded ${state.size} bundled adapters`);
  // Kick off a non-blocking refresh.
  refreshAdapters().catch((e) => log.warn("initial refresh failed", e));
}

export function getAdapterForHost(host: string): AdapterConfig | null {
  for (const adapter of state.values()) {
    for (const pattern of adapter.match) {
      // Loose match — host-only comparison, ignore path.
      try {
        const u = new URL(pattern.replace(/\*/g, "placeholder"));
        if (u.host === host) return adapter;
      } catch {
        /* continue */
      }
    }
  }
  return null;
}

export function allAdapters(): Record<string, AdapterConfig> {
  return Object.fromEntries(state.entries());
}

export async function refreshAdapters(): Promise<void> {
  const apiUrl = (await getSetting<string>("apiUrl", DEFAULT_API_URL)) || DEFAULT_API_URL;
  let manifest: { adapters?: Record<string, { url: string; adapter_version: number }> };
  try {
    const res = await fetch(`${apiUrl}/v1/extension/adapters/manifest.json`);
    if (!res.ok) throw new Error(`status ${res.status}`);
    manifest = await res.json();
  } catch (e) {
    log.warn("manifest fetch failed — keeping bundled", e);
    return;
  }
  const entries = Object.entries(manifest.adapters ?? {});
  for (const [provider, entry] of entries) {
    const current = state.get(provider);
    if (current && current.adapter_version >= entry.adapter_version) continue;
    try {
      const url = entry.url.startsWith("http") ? entry.url : `${apiUrl}${entry.url}`;
      const res = await fetch(url);
      if (!res.ok) continue;
      const envelope = await res.json();
      const verified = await verifySignedAdapter(envelope);
      if (!verified.ok) {
        log.warn(`${provider} signature check failed: ${verified.reason}`);
        continue;
      }
      state.set(provider, verified.adapter);
      log.info(`${provider} updated → v${verified.adapter.adapter_version}`);
      // Notify content scripts for hosts this adapter matches.
      for (const pattern of verified.adapter.match) {
        try {
          const host = new URL(pattern.replace(/\*/g, "placeholder")).host;
          chrome.tabs.query({ url: pattern }, (tabs) => {
            for (const tab of tabs) {
              if (!tab.id) continue;
              chrome.tabs
                .sendMessage(tab.id, {
                  kind: "adapter-updated",
                  host,
                  adapter: verified.adapter,
                })
                .catch(() => {});
            }
          });
        } catch {
          /* skip malformed pattern */
        }
      }
    } catch (e) {
      log.warn(`fetch ${provider} failed`, e);
    }
  }
}

export function setupAdapterAlarm(): void {
  chrome.alarms.create("modelstat-refresh-adapters", {
    periodInMinutes: ADAPTER_POLL_INTERVAL_MS / 60_000,
  });
}

export async function recordBreakage(
  provider: string,
  adapter_version: number,
  invariant: string,
  url_host: string,
): Promise<void> {
  await db().breakage.add({
    provider,
    adapter_version,
    invariant,
    url_host,
    reported: 0,
    ts: Date.now(),
  });
}

export async function flushBreakage(): Promise<void> {
  const apiUrl = (await getSetting<string>("apiUrl", DEFAULT_API_URL)) || DEFAULT_API_URL;
  const rows = await db().breakage.where("reported").equals(0).limit(50).toArray();
  for (const row of rows) {
    try {
      const res = await fetch(`${apiUrl}/v1/extension/telemetry/breakage`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          provider: row.provider,
          adapter_version: row.adapter_version,
          invariant: row.invariant,
          url_host: row.url_host,
        }),
      });
      if (res.ok || res.status === 429) {
        if (row.id !== undefined) {
          await db().breakage.update(row.id, { reported: 1 });
        }
      }
    } catch {
      /* retry next sweep */
    }
  }
}
