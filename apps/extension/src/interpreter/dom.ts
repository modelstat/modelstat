/**
 * DOM primitives for the interpreter.
 *
 *   - querySelector + textContent / attribute / dataset extraction
 *   - MutationObserver-based "dom.observe" that fires once per new
 *     element matching a selector (dedupes by idAttr)
 */

import type {
  AdapterConfig,
  DomObserveExtractor,
  MessagesExtractor,
  ScalarExtractor,
} from "@modelstat/adapters-protocol";
import type { DomEventPayload } from "./runtime-msgs.js";
import { extractScalarFromUrl } from "./url.js";

type ScalarField = "model" | "conversation_id";

export function extractScalarFromDom(
  extractors: ScalarExtractor[],
  field: ScalarField,
): string | null {
  for (const ex of extractors) {
    try {
      if (ex.kind === "dom.selector.text") {
        const el = document.querySelector(ex.selector);
        if (!el) continue;
        const txt = (el.textContent ?? "").trim();
        if (!txt) continue;
        if (ex.regex) {
          const m = new RegExp(ex.regex).exec(txt);
          if (!m) continue;
          const group = typeof ex.group === "string" ? ex.group : (ex.group ?? 1);
          const v = typeof group === "string" ? m.groups?.[group] : m[group];
          if (v) return v;
        } else {
          return txt;
        }
      } else if (ex.kind === "dom.selector.attr") {
        const el = document.querySelector(ex.selector);
        const v = el?.getAttribute(ex.attr);
        if (v) return v;
      } else if (ex.kind === "dom.selector.dataset") {
        const el = document.querySelector<HTMLElement>(ex.selector);
        const v = el?.dataset?.[ex.key];
        if (v) return v;
      } else if (ex.kind === "url.regexGroup") {
        const v = extractScalarFromUrl([ex], window.location.href);
        if (v) return v;
      }
      // network.* extractors are handled in the SW, not here
    } catch {
      /* malformed selector → skip variant */
    }
  }
  // Tag field for the caller; keeps lint happy about the unused param
  void field;
  return null;
}

const seenMessageIds = new Set<string>();
const observers: MutationObserver[] = [];

function emitMatch(
  el: Element,
  ex: DomObserveExtractor,
  adapter: AdapterConfig,
  push: (p: DomEventPayload) => void,
): void {
  const messageId = el.getAttribute(ex.idAttr);
  if (!messageId) return;
  const key = `${window.location.host}::${messageId}`;
  if (seenMessageIds.has(key)) return;
  seenMessageIds.add(key);

  const role: "user" | "assistant" =
    (ex.roleAttr ? (el.getAttribute(ex.roleAttr) as "user" | "assistant") : null) ??
    ex.roleDefault ??
    "assistant";

  const textNode = ex.textSelector ? el.querySelector(ex.textSelector) : el;
  const text = (textNode?.textContent ?? "").trim();
  if (!text) return;

  push({
    source: "dom-observe",
    messageId,
    role,
    text,
    host: window.location.host,
    href: window.location.href,
    conversationId: extractScalarFromUrl(
      adapter.extractors.conversation_id,
      window.location.href,
    ),
    observedAt: Date.now(),
  });
}

export function startDomObservers(
  adapter: AdapterConfig,
  push: (payload: DomEventPayload) => void,
): void {
  stopDomObservers();

  const messageExtractors = adapter.extractors.messages.filter(
    (e): e is MessagesExtractor & { kind: "dom.observe" } => e.kind === "dom.observe",
  );

  for (const ex of messageExtractors) {
    // Initial sweep — existing elements.
    document.querySelectorAll(ex.selector).forEach((el) => emitMatch(el, ex, adapter, push));

    const observer = new MutationObserver((mutations) => {
      for (const m of mutations) {
        if (m.type === "childList") {
          m.addedNodes.forEach((n) => {
            if (!(n instanceof Element)) return;
            if (n.matches(ex.selector)) emitMatch(n, ex, adapter, push);
            n.querySelectorAll(ex.selector).forEach((el) => emitMatch(el, ex, adapter, push));
          });
        } else if (m.type === "characterData") {
          // Text mutated inside an already-seen element — re-emit so the
          // SW can update the message text (streaming completion).
          const el = m.target.parentElement?.closest(ex.selector);
          if (el) {
            seenMessageIds.delete(`${window.location.host}::${el.getAttribute(ex.idAttr)}`);
            emitMatch(el, ex, adapter, push);
          }
        }
      }
    });
    observer.observe(document.body || document.documentElement, {
      childList: true,
      subtree: true,
      characterData: true,
    });
    observers.push(observer);
  }

  // Also emit current scalars (model) once on install.
  const model = extractScalarFromDom(adapter.extractors.model, "model");
  if (model) {
    push({
      source: "dom-scalar",
      host: window.location.host,
      field: "model",
      value: model,
      observedAt: Date.now(),
    });
  }
}

export function stopDomObservers(): void {
  for (const o of observers) o.disconnect();
  observers.length = 0;
  seenMessageIds.clear();
}

/** Check adapter invariants. Returns failing invariant descriptions. */
export function checkInvariants(adapter: AdapterConfig): string[] {
  const failures: string[] = [];
  for (const inv of adapter.invariants) {
    if (inv.kind === "dom.minCount") {
      const count = document.querySelectorAll(inv.selector).length;
      if (count < inv.min) {
        failures.push(`dom.minCount(${inv.selector}) want ≥${inv.min}, got ${count}`);
      }
    }
  }
  return failures;
}
