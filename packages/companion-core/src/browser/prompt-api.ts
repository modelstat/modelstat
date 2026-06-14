/**
 * Chrome built-in Prompt API adapter — Gemini Nano runs on-device
 * inside Chrome. Zero download for the user when available.
 *
 * Drop-in alternative to the WebLLM summariser. Same prompt, same
 * max-tokens cap, same 240-char output slice. Produces segments
 * indistinguishable from Ollama/WebLLM output at the pipeline level.
 *
 * Detection:
 *   - `globalThis.ai?.languageModel` present → browser supports Prompt API
 *   - `.availability()` → "readily" means the model is downloaded and ready
 *   - Anything else (not-installed, after-download, unavailable) → caller
 *     should fall through to WebLLM or no-op
 *
 * Upstream spec is still in origin trial; we probe defensively.
 */

import type { Summarizer } from "../pipeline/index.js";
import {
  SUMMARISER_MAX_TOKENS,
  SUMMARISER_SYSTEM_PROMPT,
  SUMMARISER_TEMPERATURE,
  SUMMARISER_TOP_K,
} from "../pipeline/prompts.js";
import {
  buildCognitionUserPrompt,
  COGNITION_MAX_TOKENS,
  COGNITION_SYSTEM_PROMPT,
  COGNITION_TEMPERATURE,
  parseCognitionReply,
  type Cognizer,
} from "../pipeline/cognition.js";

/** Structural view of the subset of the Prompt API we use — kept
 * loose on purpose since the spec is still evolving. */
interface PromptApiLike {
  languageModel?: {
    availability?: () => Promise<"readily" | "after-download" | "no" | string>;
    create: (opts?: {
      systemPrompt?: string;
      temperature?: number;
      topK?: number;
    }) => Promise<PromptSession>;
  };
}

interface PromptSession {
  prompt: (input: string, opts?: { maxOutputTokens?: number }) => Promise<string>;
  destroy?: () => Promise<void>;
}

function getPromptApi(): PromptApiLike | null {
  const w = globalThis as unknown as { ai?: PromptApiLike };
  return w.ai ?? null;
}

export async function promptApiReady(): Promise<boolean> {
  const api = getPromptApi();
  if (!api?.languageModel?.availability || !api.languageModel.create) return false;
  try {
    const status = await api.languageModel.availability();
    return status === "readily";
  } catch {
    return false;
  }
}

/**
 * Lazy, per-call session. The Prompt API is designed for
 * short-lived sessions; we create one per summarise call, run the
 * prompt, and dispose. Keeps memory steady and avoids leaking
 * context across unrelated sessions.
 */
export function promptApiSummarize(): Summarizer {
  return async ({ prompt, maxTokens }) => {
    const api = getPromptApi();
    if (!api?.languageModel?.create) {
      throw new Error("prompt-api-unavailable");
    }
    const session = await api.languageModel.create({
      systemPrompt: SUMMARISER_SYSTEM_PROMPT,
      temperature: SUMMARISER_TEMPERATURE,
      topK: SUMMARISER_TOP_K,
    });
    try {
      const out = await session.prompt(prompt, {
        maxOutputTokens: Math.min(maxTokens, SUMMARISER_MAX_TOKENS),
      });
      return out.trim().slice(0, 240);
    } finally {
      try {
        await session.destroy?.();
      } catch {
        /* destroy is best-effort */
      }
    }
  };
}

/**
 * Cognition pass via the Chrome Prompt API. Reuses the same lazy-
 * session pattern as promptApiSummarize. Best-effort: any error
 * returns null so the pipeline degrades gracefully.
 */
export function promptApiCognize(): Cognizer {
  return async ({ abstract }) => {
    if (!abstract || abstract.trim().length < 12) return null;
    const api = getPromptApi();
    if (!api?.languageModel?.create) return null;
    let session: Awaited<ReturnType<typeof api.languageModel.create>>;
    try {
      session = await api.languageModel.create({
        systemPrompt: COGNITION_SYSTEM_PROMPT,
        temperature: COGNITION_TEMPERATURE,
        topK: 3,
      });
    } catch {
      return null;
    }
    try {
      const out = await session.prompt(buildCognitionUserPrompt(abstract), {
        maxOutputTokens: COGNITION_MAX_TOKENS,
      });
      return parseCognitionReply(out);
    } catch {
      return null;
    } finally {
      try {
        await session.destroy?.();
      } catch {
        /* best-effort */
      }
    }
  };
}
