/**
 * Service worker entry point.
 *
 * Responsibilities:
 *   1. Register MAIN-world content script on install.
 *   2. Boot adapter registry + pricing + alarms.
 *   3. Route chrome.runtime messages between content scripts, offscreen,
 *      popup, and options.
 *   4. Drive the two-phase commit sweep + ingest queue + adapter refresh
 *      + breakage telemetry on alarms.
 *
 * SW is ephemeral (killed after 30s idle). All durable state lives in
 * IndexedDB / chrome.storage.local. Alarms are how we get control back
 * periodically.
 */

import type { Agent } from "@modelstat/core/enums";
import { createLogger } from "@/common/logger.js";
import { bump, counters, noteTabFrame, noteUrl, recentUrlsSnapshot, tabLastFrame } from "./counters.js";
import { processFrame } from "@/interpreter/network.js";
import type { DomEventPayload, RuntimeMsg } from "@/interpreter/runtime-msgs.js";
import {
  flushBreakage,
  getAdapterForHost,
  initAdapters,
  recordBreakage,
  refreshAdapters,
  setupAdapterAlarm,
} from "./adapter-registry.js";
import {
  bootAuth,
  disconnect,
  getAuthStatus,
  getClaimCode,
  getClaimUrl,
  openClaim,
  refreshSelf,
} from "./auth.js";
import { postDiscovery, setupDiscoveryAlarm } from "./discovery.js";
import { postHeartbeat, setupHeartbeatAlarm } from "./heartbeat.js";
import { flushQueue, setupIngestAlarm } from "./ingest-queue.js";
import { offscreenTokenize } from "./offscreen.js";
import { db, getSetting, setSetting } from "@/storage/db.js";
import {
  ingestDomEvent,
  ingestNetworkMessage,
  ingestScalar,
  sweepFinalise,
  markStreamEnded,
} from "@/storage/two-phase.js";
import { resolveTokenizerName } from "@/offscreen/tokenizers/index.js";
import { FIRST_IMPRESSION_QUIET_MS } from "@/common/config.js";

const log = createLogger("sw");

async function registerMainWorld(): Promise<void> {
  try {
    const scripts = await chrome.scripting.getRegisteredContentScripts({
      ids: ["modelstat-main-world"],
    });
    if (scripts.length > 0) return;
    await chrome.scripting.registerContentScripts([
      {
        id: "modelstat-main-world",
        matches: [
          "https://chatgpt.com/*",
          "https://chat.openai.com/*",
          "https://claude.ai/*",
          "https://gemini.google.com/*",
          "https://grok.com/*",
          "https://x.com/i/grok/*",
        ],
        js: ["src/content/bridge.js"],
        runAt: "document_start",
        world: "ISOLATED",
      },
    ]);
  } catch (e) {
    // Already-registered is fine.
    log.debug("registerContentScripts", e);
  }
}

function tabCtx(sender: chrome.runtime.MessageSender): { host: string; href: string } | null {
  const url = sender.url;
  if (!url) return null;
  try {
    const u = new URL(url);
    return { host: u.host, href: u.href };
  } catch {
    return null;
  }
}

async function buildCommitterCtx(host: string) {
  const adapter = getAdapterForHost(host);
  if (!adapter) return null;
  return {
    agent: adapter.provider as Agent,
    vendor: adapter.vendor,
    host,
    tokenizerBinding: adapter.tokenizer,
    requestTokenize: async (
      binding: { default: string; byModel?: Record<string, string> },
      model: string | null,
      text: string,
    ) => {
      const name = resolveTokenizerName(binding, model);
      try {
        const tokens = await offscreenTokenize(name, text);
        return {
          tokens,
          name,
          accuracy: (name.startsWith("tiktoken/") ? "exact" : "estimated") as "exact" | "estimated",
        };
      } catch {
        return { tokens: Math.ceil(text.length / 3.8), name: "fallback", accuracy: "estimated" as const };
      }
    },
    onEvent: async (event: import("@/storage/db.js").StoredEvent) => {
      await db().events.add(event);
      bump("events");
    },
  };
}

chrome.runtime.onInstalled.addListener(async () => {
  log.info("installed");
  await registerMainWorld();
  await initAdapters();
  // Silent self-register on first boot; re-hydrates on subsequent boots.
  bootAuth().catch((e) => log.warn("bootAuth failed", e));
  setupAdapterAlarm();
  setupIngestAlarm();
  setupDiscoveryAlarm();
  setupHeartbeatAlarm();
  // Poll /v1/devices/me every 10 s while the SW is awake so the
  // unclaimed → claimed flip is picked up reactively after the user
  // visits /d/:claim_code in the dashboard and claims.
  chrome.alarms.create("modelstat-refresh-self", { periodInMinutes: 1 / 6 });
  chrome.alarms.create("modelstat-sweep-finalise", { periodInMinutes: 0.25 }); // 15s
  chrome.alarms.create("modelstat-flush-breakage", { periodInMinutes: 5 });
});

chrome.runtime.onStartup.addListener(async () => {
  await registerMainWorld();
  await initAdapters();
  bootAuth().catch((e) => log.warn("bootAuth on startup failed", e));
});

chrome.alarms.onAlarm.addListener(async (alarm) => {
  try {
    if (alarm.name === "modelstat-refresh-adapters") await refreshAdapters();
    else if (alarm.name === "modelstat-flush-ingest") await flushQueue();
    else if (alarm.name === "modelstat-flush-breakage") await flushBreakage();
    else if (alarm.name === "modelstat-discovery") await postDiscovery();
    else if (alarm.name === "modelstat-heartbeat") await postHeartbeat();
    else if (alarm.name === "modelstat-refresh-self") await refreshSelf();
    else if (alarm.name === "modelstat-sweep-finalise") {
      // Sweep each host that has pending rows
      const hostRows = await db().pending.where("finalised").equals(0 as unknown as number).toArray();
      const hosts = Array.from(new Set(hostRows.map((r) => r.host)));
      for (const host of hosts) {
        const ctx = await buildCommitterCtx(host);
        if (!ctx) continue;
        await sweepFinalise(ctx);
      }
    }
  } catch (e) {
    log.warn(`alarm ${alarm.name} failed`, e);
  }
});

// ── First-impression fast path ───────────────────────────────────────────
// Until this device's first successful ship, collapse the finalise→flush wait
// so the very first session reaches the dashboard in seconds. Driven from the
// live service worker and debounced per capture; the periodic finalise/flush
// alarms remain the guaranteed fallback. Once warmed up it is a no-op, so
// steady-state capture is completely untouched.
let firstImpressionTimer: ReturnType<typeof setTimeout> | null = null;
let firstImpressionDone = false;

async function kickFirstImpression(host: string): Promise<void> {
  if (firstImpressionDone) return;
  if (await getSetting<boolean>("firstShipDone", false)) {
    firstImpressionDone = true;
    return;
  }
  if (firstImpressionTimer) clearTimeout(firstImpressionTimer);
  firstImpressionTimer = setTimeout(() => {
    firstImpressionTimer = null;
    (async () => {
      if (await getSetting<boolean>("firstShipDone", false)) {
        firstImpressionDone = true;
        return;
      }
      const ctx = await buildCommitterCtx(host);
      if (ctx) await sweepFinalise(ctx, { eager: true });
      await flushQueue({ eager: true });
    })().catch((e) => log.warn("first-impression kick failed", e));
  }, FIRST_IMPRESSION_QUIET_MS);
}

chrome.runtime.onMessage.addListener((msg: RuntimeMsg, sender, sendResponse) => {
  if (msg.kind === "network-frame") {
    bump("frames");
    if (sender.tab?.id != null) noteTabFrame(sender.tab.id);
    const ctx = tabCtx(sender);
    if (!ctx) return;
    const adapter = getAdapterForHost(ctx.host);
    if (!adapter) return;
    // Record the URL on the REQUEST frame (when we actually see it)
    // so the popup can show what's being intercepted.
    if (msg.frame.type === "request") {
      // Test against all network urlPatterns in the adapter to decide
      // whether this URL "would match" anything — lets the popup mark
      // matched vs unmatched URLs even before we see response bodies.
      const patterns: string[] = [];
      for (const ex of adapter.extractors.messages)
        if (ex.kind === "network.responseJsonPath") patterns.push(ex.urlPattern);
      for (const ex of adapter.extractors.model)
        if (ex.kind === "network.responseJsonPath" || ex.kind === "network.requestJsonPath")
          patterns.push(ex.urlPattern);
      for (const ex of adapter.extractors.conversation_id)
        if (ex.kind === "network.responseJsonPath") patterns.push(ex.urlPattern);
      let matched = false;
      for (const p of patterns) {
        try {
          if (new RegExp(p).test(msg.frame.url)) {
            matched = true;
            break;
          }
        } catch {
          /* malformed pattern */
        }
      }
      noteUrl(msg.frame.url, msg.frame.method, matched);
    }
    const out = processFrame(msg.frame, adapter, ctx.host, ctx.href);
    if (out.messages.length > 0 || out.scalars.length > 0) bump("matches");
    bump("messages", out.messages.length);
    (async () => {
      const committer = await buildCommitterCtx(ctx.host);
      if (!committer) return;
      for (const m of out.messages) {
        await ingestNetworkMessage(m, committer);
      }
      for (const s of out.scalars) {
        await ingestScalar(s);
      }
      if (msg.frame.type === "response_end") {
        // Heuristic: mark all pending from this host "stream-ended"
        // (we don't have per-message binding from network yet; safe
        // because finalise also requires dom quiet period).
        const pending = await db().pending.where({ host: ctx.host, finalised: false }).toArray();
        for (const p of pending) {
          await markStreamEnded(p.host, p.messageId);
        }
      }
      void kickFirstImpression(ctx.host);
    })().catch((e) => log.warn("frame handler", e));
    return;
  }

  if (msg.kind === "dom-event") {
    bump("dom_events");
    const payload: DomEventPayload = msg.payload;
    (async () => {
      const committer = await buildCommitterCtx(payload.host);
      if (!committer) return;
      if (payload.source === "dom-observe") {
        await ingestDomEvent(payload, committer);
      } else if (payload.source === "dom-scalar") {
        await ingestScalar({
          field: payload.field,
          value: payload.value,
          host: payload.host,
          observedAt: payload.observedAt,
        });
      } else if (payload.source === "url-change" && payload.conversationId) {
        await ingestScalar({
          field: "conversation_id",
          value: payload.conversationId,
          host: payload.host,
          observedAt: payload.observedAt,
        });
      }
      void kickFirstImpression(payload.host);
    })().catch((e) => log.warn("dom-event", e));
    return;
  }

  if (msg.kind === "get-adapter-for-host") {
    const adapter = getAdapterForHost(msg.host);
    sendResponse({ adapter });
    return true;
  }

  if (msg.kind === "popup-snapshot-request") {
    (async () => {
      const today = new Date();
      today.setHours(0, 0, 0, 0);
      const events = await db().events.where("ts").above(today.toISOString()).toArray();
      const byAgent = new Map<string, { tokens: number; model: string | null }>();
      for (const e of events) {
        const agent = e.agent;
        const prev = byAgent.get(agent) ?? { tokens: 0, model: null };
        byAgent.set(agent, {
          tokens: prev.tokens + e.input_tokens + e.output_tokens + e.reasoning_tokens,
          model: e.model ?? prev.model,
        });
      }
      const unsynced = await db().events.where("synced").equals(0 as unknown as number).count();
      const auth = getAuthStatus();
      const syncEnabled = await getSetting<boolean>("syncEnabled", true);
      const claimCode = await getClaimCode();
      const claimUrl = await getClaimUrl();

      // Session previews — most-recent first, up to 8.
      const sessions = await db()
        .sessions.orderBy("updated_at")
        .reverse()
        .limit(8)
        .toArray();

      // Pending messages (in flight): live counts per session while
      // streaming finishes and before two-phase commit finalises.
      const pendingRows = await db().pending.where("finalised").equals(0 as unknown as number).toArray();
      const pendingByConversation: Record<string, { tokens: number; count: number }> = {};
      for (const p of pendingRows) {
        const k = p.conversationId ?? p.messageId;
        const t =
          (p.usage.input ?? 0) + (p.usage.output ?? 0) + (p.usage.reasoning ?? 0);
        const cur = pendingByConversation[k] ?? { tokens: 0, count: 0 };
        pendingByConversation[k] = { tokens: cur.tokens + t, count: cur.count + 1 };
      }

      // Per-tab attach status for the currently active tab — let the
      // popup render "ATTACHED" vs "NOT ATTACHED" instead of a vague
      // "live/idle". We look it up by the active tabId the popup
      // passes along (see popup-snapshot-request in runtime-msgs).
      const activeTabId = (msg as { active_tab_id?: number }).active_tab_id;
      const activeTabLastFrame = activeTabId != null ? tabLastFrame(activeTabId) : null;

      sendResponse({
        today: Array.from(byAgent.entries()).map(([agent, v]) => ({ agent, ...v })),
        unsynced,
        auth,
        syncEnabled,
        counters,
        recent_urls: recentUrlsSnapshot(),
        active_tab_last_frame_at: activeTabLastFrame,
        claim_code: claimCode,
        claim_url: claimUrl,
        sessions: sessions.map((s) => ({
          session_id: s.session_id,
          agent: s.agent,
          model: s.model,
          conversation_id: s.conversation_id,
          tokens: s.tokens_input + s.tokens_output + s.tokens_reasoning,
          messages: s.message_count,
          updated_at: s.updated_at,
        })),
        pending_by_conversation: pendingByConversation,
      });
    })();
    return true;
  }

  if (msg.kind === "auth-open-claim") {
    openClaim();
    sendResponse({ ok: true });
    return true;
  }

  if (msg.kind === "auth-refresh") {
    refreshSelf().then((status) => sendResponse({ ok: true, status })).catch((e) => sendResponse({ error: String(e) }));
    return true;
  }

  if (msg.kind === "auth-disconnect") {
    disconnect().then(() => sendResponse({ ok: true }));
    return true;
  }

  if (msg.kind === "sync-toggle") {
    setSetting("syncEnabled", msg.enabled).then(() => sendResponse({ ok: true }));
    return true;
  }

  return false;
});

// Silence unused-import warning — recordBreakage is called in the
// content script path, but TS sees only the SW export here.
void recordBreakage;
