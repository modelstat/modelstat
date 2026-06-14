/**
 * Offscreen document — host for WASM tokenizers, ONNX embeddings, and
 * WebLLM. The SW sends it tokenize/embed/summarize requests; this
 * process does the work and replies.
 *
 * Why offscreen, not SW: service workers are routinely killed at 30s
 * idle; they can't host long-lived model instances. Offscreen docs
 * have a user-controlled lifetime (SW's offscreen-manager opens them
 * on demand, closes them when idle).
 */

import { createLogger } from "@/common/logger.js";
import type { RuntimeMsg } from "@/interpreter/runtime-msgs.js";
import { tokenize } from "./tokenizers/index.js";
import { embed } from "./local-ai/embeddings.js";
import { compact, summarize, categorizeWorkType, type AiConfig } from "./local-ai/dispatcher.js";
import { DEFAULT_WEBLLM_MODEL } from "./local-ai/webllm.js";

const log = createLogger("offscreen");

log.info("booted");

function aiConfig(): AiConfig {
  // Populated from chrome.storage (SW owns the state) — read-through.
  return {
    webllmEnabled: true,
    webllmModel: DEFAULT_WEBLLM_MODEL,
  };
}

chrome.runtime.onMessage.addListener((msg: RuntimeMsg, _sender, sendResponse) => {
  if (msg.kind === "offscreen-tokenize") {
    tokenize(msg.tokenizer, msg.text)
      .then((r) => sendResponse({ kind: "offscreen-tokenize-result", requestId: msg.requestId, tokens: r.tokens }))
      .catch((e) => {
        log.error("tokenize failed", e);
        sendResponse({ kind: "offscreen-tokenize-result", requestId: msg.requestId, tokens: 0 });
      });
    return true;
  }
  if (msg.kind === "offscreen-embed") {
    embed(msg.text)
      .then((v) => sendResponse({ kind: "offscreen-embed-result", requestId: msg.requestId, vector: v }))
      .catch((e) => {
        log.error("embed failed", e);
        sendResponse({ kind: "offscreen-embed-result", requestId: msg.requestId, vector: [] });
      });
    return true;
  }
  if (msg.kind === "offscreen-summarize") {
    summarize(msg.text, aiConfig())
      .then((r) =>
        sendResponse({
          kind: "offscreen-summarize-result",
          requestId: msg.requestId,
          summary: r.text,
          engine: r.engine,
        }),
      )
      .catch((e) => {
        log.error("summarize failed", e);
        sendResponse({
          kind: "offscreen-summarize-result",
          requestId: msg.requestId,
          summary: "",
          engine: "fallback",
        });
      });
    return true;
  }
  return false;
});

// Expose categorizeWorkType + compact over the same bus when SW wants them
// — wire them up explicitly here so we stay honest about what's exposed.
void compact;
void categorizeWorkType;
