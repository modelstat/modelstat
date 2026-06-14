/**
 * Offscreen document manager.
 *
 * Creates (and keeps alive) the offscreen document that hosts our
 * tokenizers + local-AI models. Exposes request/response helpers the
 * SW uses to call into it.
 */

import { createLogger } from "@/common/logger.js";
import type { RuntimeMsg } from "@/interpreter/runtime-msgs.js";

const log = createLogger("offscreen-mgr");
const OFFSCREEN_URL = "src/offscreen/index.html";

async function ensureOffscreen(): Promise<void> {
  const existing = await chrome.runtime.getContexts?.({
    contextTypes: ["OFFSCREEN_DOCUMENT" as chrome.runtime.ContextType],
  });
  if (existing && existing.length > 0) return;
  try {
    await chrome.offscreen.createDocument({
      url: OFFSCREEN_URL,
      reasons: ["WORKERS" as chrome.offscreen.Reason],
      justification: "Host WASM tokenizers + WebGPU inference for local-only token counting and summarisation.",
    });
  } catch (e) {
    // Races with other open calls yield "Only a single offscreen document may be created" — ignore.
    log.debug("createDocument threw (likely race)", e);
  }
}

let nextRequestId = 0;
function newRequestId(): string {
  return `${Date.now().toString(36)}-${(++nextRequestId).toString(36)}`;
}

async function request<T>(msg: RuntimeMsg, kindReply: T extends { kind: infer K } ? K : never): Promise<T> {
  await ensureOffscreen();
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("offscreen_timeout")), 60_000);
    chrome.runtime.sendMessage(msg, (reply: T) => {
      clearTimeout(timer);
      if (chrome.runtime.lastError) {
        reject(new Error(chrome.runtime.lastError.message));
        return;
      }
      if ((reply as unknown as { kind?: string })?.kind !== kindReply) {
        reject(new Error(`unexpected reply kind ${(reply as unknown as { kind?: string })?.kind}`));
        return;
      }
      resolve(reply);
    });
  });
}

export async function offscreenTokenize(
  tokenizer: string,
  text: string,
): Promise<number> {
  const requestId = newRequestId();
  const reply = await request<{ kind: "offscreen-tokenize-result"; requestId: string; tokens: number }>(
    { kind: "offscreen-tokenize", tokenizer, text, requestId },
    "offscreen-tokenize-result",
  );
  return reply.tokens;
}

export async function offscreenSummarize(text: string): Promise<{ summary: string; engine: string }> {
  const requestId = newRequestId();
  const reply = await request<{
    kind: "offscreen-summarize-result";
    requestId: string;
    summary: string;
    engine: string;
  }>({ kind: "offscreen-summarize", text, requestId }, "offscreen-summarize-result");
  return { summary: reply.summary, engine: reply.engine };
}

export async function offscreenEmbed(text: string): Promise<number[]> {
  const requestId = newRequestId();
  const reply = await request<{ kind: "offscreen-embed-result"; requestId: string; vector: number[] }>(
    { kind: "offscreen-embed", text, requestId },
    "offscreen-embed-result",
  );
  return reply.vector;
}
