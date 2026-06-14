/**
 * Unified "summarize / compact / categorize" dispatcher — tiered:
 *
 *   summarize: Chrome Prompt API → WebLLM (if user opted in) → null
 *   compact:   Chrome Prompt API → WebLLM (if user opted in) → truncate
 *   categorize: embeddings (always, 25MB MiniLM) → cosine argmax
 *
 * The SW delegates here via offscreen messaging. All work happens in
 * the offscreen document; this module never touches the network.
 */

import { createLogger } from "@/common/logger.js";
import {
  promptApiAvailability,
  promptApiCompact,
  promptApiSummarize,
} from "./prompt-api.js";
import {
  loadWebLLM,
  webllmCompact,
  webllmSummarize,
  type WebLLMModelId,
} from "./webllm.js";
import { classifyZeroShot } from "./embeddings.js";

const log = createLogger("ai-dispatcher");

export type AiConfig = {
  webllmEnabled: boolean;
  webllmModel: WebLLMModelId;
};

export type Engine = "prompt-api" | "webllm" | "fallback" | "embedding";

export type SummarizeResult = {
  text: string;
  engine: Engine;
};

export async function summarize(text: string, cfg: AiConfig): Promise<SummarizeResult> {
  const nano = await promptApiAvailability();
  if (nano === "available") {
    try {
      const out = await promptApiSummarize(text);
      if (out) return { text: out, engine: "prompt-api" };
    } catch (e) {
      log.warn("prompt-api summarize failed", e);
    }
  }
  if (cfg.webllmEnabled) {
    try {
      await loadWebLLM(cfg.webllmModel);
      const out = await webllmSummarize(text);
      if (out) return { text: out, engine: "webllm" };
    } catch (e) {
      log.warn("webllm summarize failed", e);
    }
  }
  // Naive fallback: first sentence capped at 200 chars. Keeps the UX
  // informative without any model.
  const head = text.slice(0, 200).split(/[.!?]\s/)[0] ?? text.slice(0, 120);
  return { text: head + (text.length > head.length ? "…" : ""), engine: "fallback" };
}

export async function compact(
  text: string,
  maxTokens: number,
  cfg: AiConfig,
): Promise<SummarizeResult> {
  const nano = await promptApiAvailability();
  if (nano === "available") {
    try {
      const out = await promptApiCompact(text, maxTokens);
      if (out) return { text: out, engine: "prompt-api" };
    } catch (e) {
      log.warn("prompt-api compact failed", e);
    }
  }
  if (cfg.webllmEnabled) {
    try {
      await loadWebLLM(cfg.webllmModel);
      const out = await webllmCompact(text, maxTokens);
      if (out) return { text: out, engine: "webllm" };
    } catch (e) {
      log.warn("webllm compact failed", e);
    }
  }
  // Fallback: head-and-tail truncation (preserves framing).
  const approxChars = maxTokens * 4;
  if (text.length <= approxChars) return { text, engine: "fallback" };
  const head = text.slice(0, Math.floor(approxChars * 0.6));
  const tail = text.slice(text.length - Math.floor(approxChars * 0.4));
  return { text: `${head}\n…\n${tail}`, engine: "fallback" };
}

/** Map a text blob to one of modelstat's WORK_TYPES. Uses embeddings
 * (always available) — no LLM required. */
export async function categorizeWorkType(text: string): Promise<{
  label: string;
  score: number;
  engine: Engine;
}> {
  const prototypes: Record<string, string> = {
    planning: "Designing architecture, writing a plan, breaking a task into steps, clarifying requirements.",
    implementation: "Writing new code, adding a feature, implementing a function or component.",
    debugging: "Investigating a bug, reading error messages, tracing why something fails.",
    review: "Reading code, reviewing a pull request, giving feedback on another person's work.",
    refactoring: "Renaming, restructuring, moving code without changing behavior.",
    testing: "Writing unit tests, integration tests, end-to-end tests; running a test suite.",
    docs: "Writing documentation, README updates, API docs, user-facing guides.",
    devops: "CI/CD, deployment scripts, Docker, Kubernetes, infrastructure-as-code.",
    research: "Reading a paper, comparing libraries, learning a new concept, benchmarking.",
    ops: "Monitoring alerts, incident response, on-call, diagnosing production issues.",
    chat: "General conversation, small talk, non-technical discussion.",
    misc: "Miscellaneous tasks that don't fit other categories.",
  };
  const { label, score } = await classifyZeroShot(text, prototypes);
  return { label, score, engine: "embedding" };
}
