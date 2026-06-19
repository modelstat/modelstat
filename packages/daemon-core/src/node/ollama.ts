/**
 * Ollama adapters for the daemon pipeline on Node (CLI + worker).
 *
 * Binds redact → segment → summarise → tag to a local Ollama daemon.
 * Models are the same Qwen family used across runtimes — see
 * packages/daemon-core/src/pipeline/prompts.ts for the canonical
 * contract. The CLI/Ollama path runs the full 4B; the browser
 * extension runs a smaller in-browser variant under WebGPU limits.
 *
 *   embeddings     : bge-small-en-v1.5        (384 dims, ~33 MB on disk)
 *   summarisation  : qwen3:4b                  (~2.5 GB on disk)
 *
 * The Ollama daemon is expected to be reachable at env.OLLAMA_URL
 * (default http://localhost:11434) with both models pulled.
 */

import {
  OLLAMA_CHAT_MODEL,
  OLLAMA_EMBED_MODEL,
  QWEN_CHARS_PER_TOKEN,
  SUMMARISER_MAX_TOKENS,
  SUMMARISER_SYSTEM_PROMPT,
  SUMMARISER_TEMPERATURE,
} from "../pipeline/prompts.js";
import {
  buildCognitionUserPrompt,
  COGNITION_MAX_TOKENS,
  COGNITION_SYSTEM_PROMPT,
  COGNITION_TEMPERATURE,
  parseCognitionReply,
  type Cognizer,
} from "../pipeline/cognition.js";

export interface OllamaConfig {
  baseUrl: string;
  embedModel: string;
  chatModel: string;
}

export function defaultOllamaConfig(): OllamaConfig {
  const base = (globalThis as { process?: { env?: Record<string, string | undefined> } })
    .process?.env ?? {};
  return {
    baseUrl: base.OLLAMA_URL ?? "http://localhost:11434",
    embedModel: base.OLLAMA_EMBED_MODEL ?? OLLAMA_EMBED_MODEL,
    chatModel: base.OLLAMA_CHAT_MODEL ?? OLLAMA_CHAT_MODEL,
  };
}

/** Embedding adapter — returns a unit-norm 384-dim vector for `text`. */
export function ollamaEmbed(cfg: OllamaConfig = defaultOllamaConfig()): (text: string) => Promise<number[]> {
  return async (text: string): Promise<number[]> => {
    const res = await fetch(`${cfg.baseUrl.replace(/\/+$/, "")}/api/embeddings`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: cfg.embedModel, prompt: text }),
    });
    if (!res.ok) {
      throw new Error(`ollama embed failed: ${res.status} ${await res.text().catch(() => "")}`);
    }
    const body = (await res.json()) as { embedding: number[] };
    const v = body.embedding ?? [];
    // Normalise to unit length so cosine reduces to dot product downstream.
    let norm = 0;
    for (const x of v) norm += x * x;
    norm = Math.sqrt(norm) || 1;
    return v.map((x) => x / norm);
  };
}

/** Tokenizer — cheap approximation tuned for Qwen-family tokenizers.
 * Shared ratio with the browser fallback so token counts are
 * directionally comparable across runtimes. */
export function ollamaTokenize(): (text: string) => number {
  return (text: string): number => Math.max(1, Math.ceil(text.length / QWEN_CHARS_PER_TOKEN));
}

/** Summariser — short one-line abstract from pre-redacted content.
 * Returns ≤ 240 chars so the 512-char Segment cap has room for caller-added
 * context (e.g. "work on ${repo}").
 *
 * Uses Ollama /api/chat in non-streaming mode. Temperature kept low so
 * the same turns yield the same abstract on replay (helps idempotency
 * assertions in tests). */
export function ollamaSummarize(
  cfg: OllamaConfig = defaultOllamaConfig(),
): (input: { prompt: string; maxTokens: number }) => Promise<string> {
  return async ({ prompt, maxTokens }) => {
    const res = await fetch(`${cfg.baseUrl.replace(/\/+$/, "")}/api/chat`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        model: cfg.chatModel,
        stream: false,
        // Disable reasoning. qwen3 (the default summariser family) is a
        // thinking model: with `think` on it spends the entire
        // `num_predict` budget on a <think> block and returns EMPTY
        // content, so the summariser saw "" and the whole pipeline
        // crash-looped at preflight. We only want the final terse
        // abstract, never the chain-of-thought. Ollama ignores this
        // field for non-thinking models, so it's safe across families.
        think: false,
        options: {
          temperature: SUMMARISER_TEMPERATURE,
          num_predict: Math.min(maxTokens, SUMMARISER_MAX_TOKENS),
        },
        messages: [
          { role: "system", content: SUMMARISER_SYSTEM_PROMPT },
          { role: "user", content: prompt },
        ],
      }),
    });
    if (!res.ok) {
      throw new Error(`ollama summarize failed: ${res.status} ${await res.text().catch(() => "")}`);
    }
    const body = (await res.json()) as { message?: { content?: string } };
    return (body.message?.content ?? "").trim().slice(0, 240);
  };
}

/**
 * Cognition adapter — small "what mood + mode is the user in?" pass
 * that runs after the summariser. Same Qwen3.5 model, different
 * system prompt, JSON output. Best-effort: any failure returns null
 * and the segment ships without a cognition suffix. See
 * pipeline/cognition.ts for the contract.
 */
export function ollamaCognize(
  cfg: OllamaConfig = defaultOllamaConfig(),
): Cognizer {
  return async ({ abstract }) => {
    if (!abstract || abstract.trim().length < 12) return null;
    let res: Response;
    try {
      res = await fetch(`${cfg.baseUrl.replace(/\/+$/, "")}/api/chat`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          model: cfg.chatModel,
          stream: false,
          format: "json",
          // Same reason as the summariser: no thinking budget, just the
          // JSON cognition tags. Thinking models otherwise emit a long
          // <think> block and return empty content.
          think: false,
          options: {
            temperature: COGNITION_TEMPERATURE,
            num_predict: COGNITION_MAX_TOKENS,
          },
          messages: [
            { role: "system", content: COGNITION_SYSTEM_PROMPT },
            { role: "user", content: buildCognitionUserPrompt(abstract) },
          ],
        }),
      });
    } catch {
      return null;
    }
    if (!res.ok) return null;
    let body: { message?: { content?: string } };
    try {
      body = (await res.json()) as { message?: { content?: string } };
    } catch {
      return null;
    }
    return parseCognitionReply(body.message?.content ?? "");
  };
}
