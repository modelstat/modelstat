/**
 * Runtime message shapes for content ↔ SW ↔ offscreen ↔ popup/options.
 * Every chrome.runtime.sendMessage payload in this extension is a
 * discriminated union on `kind`.
 */

import type { AdapterConfig } from "@modelstat/adapters-protocol";
import type { MainFrame } from "@/content/bridge.js";

export type DomEventPayload =
  | {
      source: "dom-observe";
      messageId: string;
      role: "user" | "assistant";
      text: string;
      host: string;
      href: string;
      conversationId: string | null;
      observedAt: number;
    }
  | {
      source: "url-change";
      host: string;
      href: string;
      conversationId: string | null;
      observedAt: number;
    }
  | {
      source: "dom-scalar";
      host: string;
      field: "model" | "conversation_id";
      value: string;
      observedAt: number;
    };

export type RuntimeMsg =
  | { kind: "network-frame"; frame: MainFrame }
  | { kind: "dom-event"; payload: DomEventPayload }
  | { kind: "get-adapter-for-host"; host: string }
  | { kind: "adapter-updated"; host: string; adapter: AdapterConfig }
  | { kind: "popup-snapshot-request"; active_tab_id?: number }
  | { kind: "offscreen-tokenize"; tokenizer: string; text: string; requestId: string }
  | { kind: "offscreen-tokenize-result"; requestId: string; tokens: number }
  | { kind: "offscreen-embed"; text: string; requestId: string }
  | { kind: "offscreen-embed-result"; requestId: string; vector: number[] }
  | { kind: "offscreen-summarize"; text: string; requestId: string; maxTokens?: number }
  | { kind: "offscreen-summarize-result"; requestId: string; summary: string; engine: string }
  | { kind: "auth-open-claim" }
  | { kind: "auth-refresh" }
  | { kind: "auth-disconnect" }
  | { kind: "sync-toggle"; enabled: boolean };
