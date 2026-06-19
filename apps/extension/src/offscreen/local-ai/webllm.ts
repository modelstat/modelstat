/**
 * WebLLM engine wrapper — runs a local Qwen via WebGPU as the fallback
 * summariser when Chrome's built-in Prompt API (Gemini Nano) isn't
 * available. Opt-in: the user must accept the download in Options first.
 *
 * The default is the SAME model the CLI bundles — Qwen3-4B-Instruct, kept
 * in sync via daemon-core's `WEBLLM_CHAT_MODEL` — so an
 * extension-produced abstract matches a CLI-produced one. It's heavier
 * (~2.4 GB WebGPU heap); the lighter ids stay selectable for constrained
 * GPUs, and if a model can't load the dispatcher falls through to the
 * Prompt API or a naive truncation (graceful, never fatal).
 *
 * Weights are cached by WebLLM in IndexedDB/OPFS automatically; the
 * second launch is instant.
 */

import type { InitProgressReport, MLCEngine } from "@mlc-ai/web-llm";
import { WEBLLM_CHAT_MODEL } from "@modelstat/daemon-core/pipeline";
import { createLogger } from "@/common/logger.js";

const log = createLogger("webllm");

export type WebLLMModelId =
  | "Qwen3-4B-Instruct-q4f16_1-MLC" // default — same as the CLI (WEBLLM_CHAT_MODEL)
  | "Qwen3-0.6B-Instruct-q4f16_1-MLC" // lighter fallback for constrained GPUs
  | "SmolLM2-360M-Instruct-q4f16_1-MLC"; // smallest fallback

/** Default WebLLM model — the CLI's Qwen, via the shared constant so the
 * extension can't drift from the CLI/canonical contract again. */
export const DEFAULT_WEBLLM_MODEL: WebLLMModelId = WEBLLM_CHAT_MODEL;

export type DownloadProgress = {
  progress: number; // 0..1
  text: string;
  modelId: WebLLMModelId;
};

let enginePromise: Promise<MLCEngine> | null = null;
let activeModel: WebLLMModelId | null = null;

export async function loadWebLLM(
  modelId: WebLLMModelId,
  onProgress?: (p: DownloadProgress) => void,
): Promise<MLCEngine> {
  if (enginePromise && activeModel === modelId) return enginePromise;
  const { CreateMLCEngine } = await import("@mlc-ai/web-llm");
  activeModel = modelId;
  enginePromise = CreateMLCEngine(modelId, {
    initProgressCallback: (r: InitProgressReport) => {
      onProgress?.({ progress: r.progress, text: r.text, modelId });
      log.debug(`[${modelId}] ${r.text} (${Math.round(r.progress * 100)}%)`);
    },
  });
  return enginePromise;
}

export async function unloadWebLLM(): Promise<void> {
  if (!enginePromise) return;
  try {
    const engine = await enginePromise;
    await engine.unload();
  } catch (e) {
    log.warn("unload failed", e);
  }
  enginePromise = null;
  activeModel = null;
}

export async function webllmSummarize(text: string): Promise<string> {
  if (!enginePromise) throw new Error("webllm_not_loaded");
  const engine = await enginePromise;
  const res = await engine.chat.completions.create({
    messages: [
      {
        role: "system",
        content:
          "You are a terse summariser. Output a single 1-2 sentence description. No preamble.",
      },
      { role: "user", content: `Chat excerpt:\n${text}\n\nSummary:` },
    ],
    temperature: 0.3,
    max_tokens: 80,
    stream: false,
  });
  return res.choices[0]?.message?.content?.trim() ?? "";
}

export async function webllmCompact(text: string, maxTokens: number): Promise<string> {
  if (!enginePromise) throw new Error("webllm_not_loaded");
  const engine = await enginePromise;
  const res = await engine.chat.completions.create({
    messages: [
      {
        role: "system",
        content: `Compress the input to at most ${maxTokens} tokens while preserving factual content. Output only the compressed text.`,
      },
      { role: "user", content: text },
    ],
    temperature: 0.2,
    max_tokens: maxTokens,
    stream: false,
  });
  return res.choices[0]?.message?.content?.trim() ?? "";
}

/** Head-to-head benchmark harness — runs the same prompt through both
 * candidate models and returns side-by-side outputs + latencies. Used
 * from Options → "Benchmark summarisers" button. */
export async function benchmarkSummarizers(
  prompt: string,
  onProgress?: (p: DownloadProgress) => void,
): Promise<{
  qwen: { text: string; latencyMs: number };
  smollm: { text: string; latencyMs: number };
}> {
  const q0 = performance.now();
  await loadWebLLM(DEFAULT_WEBLLM_MODEL, onProgress);
  const qText = await webllmSummarize(prompt);
  const q1 = performance.now();
  await unloadWebLLM();

  const s0 = performance.now();
  await loadWebLLM("SmolLM2-360M-Instruct-q4f16_1-MLC", onProgress);
  const sText = await webllmSummarize(prompt);
  const s1 = performance.now();
  await unloadWebLLM();

  return {
    qwen: { text: qText, latencyMs: q1 - q0 },
    smollm: { text: sText, latencyMs: s1 - s0 },
  };
}
