/**
 * URL primitives for the interpreter.
 *
 *   - match/regex-group extraction from current URL
 *   - SPA URL change watcher (history.pushState / replaceState patches +
 *     popstate/hashchange) so we can refresh conversation_id
 */

import type { AdapterConfig } from "@modelstat/adapters-protocol";
import type { DomEventPayload } from "./runtime-msgs.js";

type Extractor = AdapterConfig["extractors"]["conversation_id"][number];

export function extractScalarFromUrl(
  extractors: Extractor[],
  href: string,
): string | null {
  for (const ex of extractors) {
    if (ex.kind !== "url.regexGroup") continue;
    try {
      const m = new RegExp(ex.pattern).exec(href);
      if (!m) continue;
      const group = typeof ex.group === "string" ? ex.group : (ex.group ?? 1);
      const value = typeof group === "string" ? m.groups?.[group] : m[group];
      if (value) return value;
    } catch {
      /* malformed regex → skip variant */
    }
  }
  return null;
}

let stopFn: (() => void) | null = null;

export function startUrlWatcher(
  adapter: AdapterConfig,
  push: (payload: DomEventPayload) => void,
): void {
  stopUrlWatcher();

  const emit = () => {
    const conversationId = extractScalarFromUrl(
      adapter.extractors.conversation_id,
      window.location.href,
    );
    push({
      source: "url-change",
      host: window.location.host,
      href: window.location.href,
      conversationId,
      observedAt: Date.now(),
    });
  };

  // Patch pushState / replaceState so we see SPA navigations. We
  // store the originals on a symbol-keyed slot; if another extension
  // already patched, we chain.
  const slot = Symbol.for("modelstat.history.orig");
  type WithSlot = History & { [k: symbol]: { push: typeof history.pushState; replace: typeof history.replaceState } };
  const h = history as WithSlot;
  if (!h[slot]) {
    h[slot] = { push: history.pushState, replace: history.replaceState };
    history.pushState = function patched(...args: Parameters<History["pushState"]>) {
      const r = h[slot]!.push.apply(this, args);
      window.dispatchEvent(new Event("modelstat:url"));
      return r;
    };
    history.replaceState = function patched(...args: Parameters<History["replaceState"]>) {
      const r = h[slot]!.replace.apply(this, args);
      window.dispatchEvent(new Event("modelstat:url"));
      return r;
    };
  }

  window.addEventListener("modelstat:url", emit);
  window.addEventListener("popstate", emit);
  window.addEventListener("hashchange", emit);

  stopFn = () => {
    window.removeEventListener("modelstat:url", emit);
    window.removeEventListener("popstate", emit);
    window.removeEventListener("hashchange", emit);
  };

  emit();
}

export function stopUrlWatcher(): void {
  stopFn?.();
  stopFn = null;
}
