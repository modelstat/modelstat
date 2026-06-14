/**
 * ISOLATED-world content script.
 *
 * Responsibilities:
 *   1. Inject the MAIN-world script (bridge.ts) before the page boots.
 *   2. Receive captured network frames from MAIN; forward to SW.
 *   3. Run DOM observers / URL extractors locally (ISOLATED has DOM
 *      access) and forward DOM events to SW.
 *   4. Request the current adapter from SW on boot; re-install DOM
 *      observers when it changes.
 *
 * Heavy lifting (interpreter merge, tokenization, storage, ingest)
 * happens in the SW — content script stays small and side-effect-free.
 */

import { installMainWorld, onFrame, type MainFrame } from "./bridge.js";
import { createLogger } from "@/common/logger.js";
import type { DomEventPayload, RuntimeMsg } from "@/interpreter/runtime-msgs.js";
import { startDomObservers, stopDomObservers } from "@/interpreter/dom.js";
import { startUrlWatcher } from "@/interpreter/url.js";

const log = createLogger("content");

// When the extension is reloaded mid-session, every old content script
// instance becomes orphaned: its chrome.runtime handle is bound to an
// SW that no longer exists and every send throws "Extension context
// invalidated". Detect that once, tear everything down, mark the tab
// so we don't loop, and reload the page so a fresh content script can
// bind to the new SW.
let contextInvalidated = false;
function handleSendError(err: unknown): void {
  if (contextInvalidated) return;
  const msg = String((err as { message?: string } | null)?.message ?? err ?? "");
  if (!msg.includes("Extension context invalidated")) return;
  contextInvalidated = true;
  stopDomObservers();
  try {
    // One-shot reload guard — sessionStorage survives the reload but
    // not the tab close, so we won't infinite-loop if reload itself
    // somehow races the new extension load.
    const k = "__modelstat_reloaded_once__";
    if (sessionStorage.getItem(k) !== "1") {
      sessionStorage.setItem(k, "1");
      location.reload();
      return;
    }
  } catch {
    /* sessionStorage can throw on sandboxed / blob: contexts — fall through */
  }
  log.warn("context invalidated, stopped observers (reload the tab to re-attach)");
}

function safeSend(msg: RuntimeMsg): void {
  try {
    chrome.runtime.sendMessage<RuntimeMsg, void>(msg).catch(handleSendError);
  } catch (e) {
    handleSendError(e);
  }
}

async function boot(): Promise<void> {
  // Clear the one-shot reload guard on a successful boot — next time
  // the extension reloads we're allowed to self-reload again.
  try {
    sessionStorage.removeItem("__modelstat_reloaded_once__");
  } catch {
    /* ignore */
  }

  try {
    await installMainWorld();
  } catch (e) {
    log.warn("MAIN-world install failed (will retry on next navigation)", e);
  }

  onFrame((frame: MainFrame) => {
    safeSend({ kind: "network-frame", frame });
  });

  const pushDomEvent = (payload: DomEventPayload) => {
    safeSend({ kind: "dom-event", payload });
  };

  const reinstall = async () => {
    if (contextInvalidated) return;
    stopDomObservers();
    let res: { adapter: unknown } | null = null;
    try {
      res = await chrome.runtime.sendMessage<RuntimeMsg, { adapter: unknown } | null>({
        kind: "get-adapter-for-host",
        host: window.location.host,
      });
    } catch (e) {
      handleSendError(e);
      return;
    }
    const adapter = res?.adapter as import("@modelstat/adapters-protocol").AdapterConfig | null;
    if (!adapter) {
      log.info("no adapter for", window.location.host);
      return;
    }
    startDomObservers(adapter, pushDomEvent);
    startUrlWatcher(adapter, pushDomEvent);
  };

  await reinstall();

  // Adapters can be pushed by the SW when a hot-update lands.
  try {
    chrome.runtime.onMessage.addListener((msg: RuntimeMsg) => {
      if (msg.kind === "adapter-updated" && msg.host === window.location.host) {
        reinstall().catch((e) => log.warn("reinstall failed", e));
      }
    });
  } catch (e) {
    handleSendError(e);
  }
}

boot().catch((e) => log.error("boot failed", e));
