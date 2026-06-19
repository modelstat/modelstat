/**
 * WebLLM adapter — pairs 1:1 with node/ollama.ts.
 *
 * Runs Qwen3-4B-Instruct (WEBLLM_CHAT_MODEL) in the user's browser via
 * @mlc-ai/web-llm (WebGPU backend, WebAssembly fallback). Same model
 * family + same prompt template as the CLI so segments produced by the
 * extension look indistinguishable from CLI-produced ones.
 *
 * Loading the model is a one-time ~2.4 GB WebGPU download (cached in the
 * browser's storage across sessions). Users who don't want the
 * download can set webllmEnabled=false in the options page — the
 * browser dispatcher falls through to a no-op summariser and the
 * pipeline emits segments with empty abstracts but the same tags.
 *
 * This module is runtime-agnostic in the sense that it doesn't import
 * @mlc-ai/web-llm at the top level — the consumer passes an MLCEngine
 * instance or a factory. Lets us tree-shake the 5 MB web-llm runtime
 * out of the SW bundle for users who never opt in.
 */

import type { Embedder, Summarizer, Tokenizer } from "../pipeline/index.js";
import {
  QWEN_CHARS_PER_TOKEN,
  SUMMARISER_MAX_TOKENS,
  SUMMARISER_SYSTEM_PROMPT,
  SUMMARISER_TEMPERATURE,
  WEBLLM_CHAT_MODEL,
} from "../pipeline/prompts.js";
import {
  buildCognitionUserPrompt,
  COGNITION_MAX_TOKENS,
  COGNITION_SYSTEM_PROMPT,
  COGNITION_TEMPERATURE,
  parseCognitionReply,
  type Cognizer,
} from "../pipeline/cognition.js";

/** Shape we need from @mlc-ai/web-llm's MLCEngine. Kept structural so
 * we don't take a hard dependency on the package from daemon-core
 * — the extension imports the real thing and passes it in. */
export interface WebLLMChatLike {
  chat: {
    completions: {
      create(req: {
        messages: Array<{ role: "system" | "user" | "assistant"; content: string }>;
        temperature: number;
        max_tokens: number;
        stream: false;
      }): Promise<{ choices: Array<{ message?: { content?: string | null } | null }> }>;
    };
  };
}

export interface WebLLMConfig {
  /** Canonical MLC model id — defaults to the family shared with Ollama. */
  chatModel: string;
}

export function defaultWebLLMConfig(): WebLLMConfig {
  return { chatModel: WEBLLM_CHAT_MODEL };
}

/**
 * Bind the summariser to a WebLLM engine. The engine MUST already be
 * initialised on `cfg.chatModel` (loading is async + progress-heavy,
 * so callers manage that themselves + surface progress to the user).
 *
 * Shape matches ollamaSummarize(): same system prompt, same temp,
 * same max-tokens cap, same 240-char output slice so segments from
 * the two runtimes pass each other's idempotency tests.
 */
export function webllmSummarize(
  engine: WebLLMChatLike,
): Summarizer {
  return async ({ prompt, maxTokens }) => {
    const res = await engine.chat.completions.create({
      messages: [
        { role: "system", content: SUMMARISER_SYSTEM_PROMPT },
        { role: "user", content: prompt },
      ],
      temperature: SUMMARISER_TEMPERATURE,
      max_tokens: Math.min(maxTokens, SUMMARISER_MAX_TOKENS),
      stream: false,
    });
    return (res.choices[0]?.message?.content ?? "").trim().slice(0, 240);
  };
}

/**
 * Cognition adapter — same shape as webllmSummarize, different system
 * prompt + JSON output. Best-effort; returns null on any failure so
 * the pipeline ships the segment without cognition rather than
 * blocking. See pipeline/cognition.ts for the contract.
 */
export function webllmCognize(engine: WebLLMChatLike): Cognizer {
  return async ({ abstract }) => {
    if (!abstract || abstract.trim().length < 12) return null;
    try {
      const res = await engine.chat.completions.create({
        messages: [
          { role: "system", content: COGNITION_SYSTEM_PROMPT },
          { role: "user", content: buildCognitionUserPrompt(abstract) },
        ],
        temperature: COGNITION_TEMPERATURE,
        max_tokens: COGNITION_MAX_TOKENS,
        stream: false,
      });
      return parseCognitionReply(res.choices[0]?.message?.content ?? "");
    } catch {
      return null;
    }
  };
}

/** Qwen-family char heuristic — same ratio as ollamaTokenize() so
 * spend attribution is comparable across runtimes when the browser
 * can't load tiktoken (CSP restrictions, missing WebAssembly, etc.). */
export function webllmTokenize(): Tokenizer {
  return (text: string) => Math.max(1, Math.ceil(text.length / QWEN_CHARS_PER_TOKEN));
}

/** Structural interface for the embedding side — the extension's
 * offscreen document runs transformers.js's Xenova/bge-small-en-v1.5
 * (same weights as Ollama's bge-small-en-v1.5). Callers pass in a
 * thin wrapper around `pipeline('feature-extraction', …)`. */
export interface EmbeddingSession {
  embed(text: string): Promise<number[]>;
}

export function webllmEmbed(session: EmbeddingSession): Embedder {
  return async (text: string) => {
    const v = await session.embed(text);
    // Normalise defensively — transformers.js sometimes returns unit
    // vectors, sometimes not, depending on model config.
    let norm = 0;
    for (const x of v) norm += x * x;
    norm = Math.sqrt(norm) || 1;
    return v.map((x) => x / norm);
  };
}
