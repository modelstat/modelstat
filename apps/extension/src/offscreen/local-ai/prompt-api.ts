/**
 * Chrome built-in Prompt API (Gemini Nano) wrapper.
 *
 *   https://developer.chrome.com/docs/ai/built-in
 *
 * Runs locally on-device with zero bundle cost when available. We use
 * it as the primary summarize/compact path; fall back to WebLLM if
 * the user's Chrome doesn't have it (yet / at all).
 *
 * The API has gone through several renames (window.ai → ai.* →
 * LanguageModel). This wrapper feature-detects all shapes and
 * exposes a single uniform interface.
 */

import {
  SUMMARISER_SYSTEM_PROMPT,
  SUMMARISER_TEMPERATURE,
  SUMMARISER_TOP_K,
} from "@modelstat/companion-core/pipeline";
import { createLogger } from "@/common/logger.js";

const log = createLogger("prompt-api");

// The API shape is still in origin trial; type it loosely.
type NanoSession = {
  prompt(input: string): Promise<string>;
  promptStreaming?(input: string): AsyncIterable<string>;
  destroy?(): void;
};

type NanoNamespace = {
  availability?(): Promise<"unavailable" | "downloadable" | "downloading" | "available">;
  capabilities?(): Promise<{ available: "readily" | "after-download" | "no" }>;
  create(opts?: { systemPrompt?: string; temperature?: number; topK?: number }): Promise<NanoSession>;
};

function resolveNamespace(): NanoNamespace | null {
  const g = globalThis as unknown as {
    LanguageModel?: NanoNamespace;
    ai?: { languageModel?: NanoNamespace };
    self?: { ai?: { languageModel?: NanoNamespace } };
  };
  return g.LanguageModel ?? g.ai?.languageModel ?? g.self?.ai?.languageModel ?? null;
}

export type PromptAvailability = "unavailable" | "downloadable" | "downloading" | "available";

export async function promptApiAvailability(): Promise<PromptAvailability> {
  const ns = resolveNamespace();
  if (!ns) return "unavailable";
  try {
    if (ns.availability) return await ns.availability();
    if (ns.capabilities) {
      const caps = await ns.capabilities();
      if (caps.available === "readily") return "available";
      if (caps.available === "after-download") return "downloadable";
      return "unavailable";
    }
  } catch (e) {
    log.warn("availability probe threw", e);
  }
  return "unavailable";
}

let sessionPromise: Promise<NanoSession> | null = null;

async function getSession(systemPrompt?: string): Promise<NanoSession> {
  if (sessionPromise) return sessionPromise;
  const ns = resolveNamespace();
  if (!ns) throw new Error("prompt_api_unavailable");
  sessionPromise = ns.create({
    systemPrompt,
    temperature: SUMMARISER_TEMPERATURE,
    topK: SUMMARISER_TOP_K,
  });
  return sessionPromise;
}

export async function promptApiSummarize(text: string): Promise<string> {
  // Canonical system prompt — shared with the Ollama / WebLLM summarisers so
  // every runtime produces near-identical abstracts.
  const session = await getSession(SUMMARISER_SYSTEM_PROMPT);
  return session.prompt(`Chat excerpt:\n${text}\n\nSummary:`);
}

export async function promptApiCompact(text: string, maxTokens: number): Promise<string> {
  const session = await getSession(
    `Compress the following text to at most ${maxTokens} tokens while preserving all factual content. Output only the compressed text — no preamble.`,
  );
  return session.prompt(text);
}
