/**
 * Reasoning-model output handling, shared by every Node LLM runtime
 * (the bundled `node-llama-cpp` path in ../node/llama.ts and the remote
 * OpenAI-compatible path in ../node/openai-compat.ts).
 *
 * Qwen3.5 / deepseek-r1 / o-series style models emit a `<think>…</think>`
 * scratchpad before the answer. Both runtimes ask the same model family
 * the same prompts, so they must strip that block identically — keeping
 * this in one place means a fix to the tag handling lands on every
 * runtime at once.
 */

/**
 * Token headroom added on top of an auxiliary pass's answer budget so a
 * reasoning model has room to emit its `<think>` block before the
 * answer (which {@link stripThinking} removes). The visible answers are
 * tiny (~30-40 tokens) but the reasoning pass routinely runs 200-400.
 */
export const THINKING_HEADROOM_TOKENS = 400;

/**
 * Strip `<think>…</think>` reasoning blocks (and a trailing unclosed
 * one) from a chat reply, returning only the answer. Tolerates
 * malformed/unclosed tags by also removing a dangling open tag.
 */
export function stripThinking(text: string): string {
  return text
    .replace(/<think>[\s\S]*?<\/think>/gi, "")
    .replace(/<think>[\s\S]*$/i, "")
    .trim();
}
