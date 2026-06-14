/**
 * Shared prompt + model contracts for the pipeline's summariser.
 *
 * Every companion runtime (Ollama on Node, WebLLM / Chrome Prompt API
 * in the browser extension) reuses these constants so the abstract
 * you get from qwen3:4b running in Ollama is almost identical to
 * what you get from Qwen3-4B-Instruct-q4f16_1-MLC in the extension
 * — same model family, same system prompt, same output cap.
 *
 * If you change the wording here, EVERY runtime's behaviour shifts at
 * once. That's the point.
 */

/**
 * Canonical summariser model identifier. Concrete runtimes map this
 * to their own identifier scheme:
 *
 *   Ollama (CLI/tray) → "qwen3:4b"                     — 4B params, ~2.5 GB q4
 *   WebLLM (browser)  → "Qwen3-4B-Instruct-q4f16_1-MLC" — same family, browser-quantised
 *   Bundled llama.cpp → Qwen3.5-4B-Q4_K_M.gguf          — agent-dev fallback when Ollama absent
 *   Prompt API        → n/a (Chrome picks its built-in Gemini Nano when available)
 *
 * Why the bump from Qwen3.5-0.8B → Qwen3-4B (May 2026):
 *   • The 0.8B model was hitting quality ceilings on multi-paragraph
 *     summarisation. Empirically the abstracts coming out of 0.8B
 *     truncated key context and routinely missed the SUBJECT of a
 *     session (the very thing downstream consumers need).
 *   • Qwen3-4B-Instruct lands near Qwen2.5-32B-Instruct on summarisation
 *     benchmarks while still running on consumer hardware (8 GB RAM,
 *     CPU-only is fine for the segment cadence — one call per session).
 *   • Browser memory is the binding constraint for WebLLM. 4B q4f16
 *     loads in ~2.4 GB of WebGPU heap; tested OK on M-series Macs and
 *     mid-range PCs. Users with extreme constraints can fall back to
 *     Chrome's built-in Prompt API path.
 *   • Only the summariser producing the abstract gets the upgrade.
 *     Higher-quality input → higher-quality concept extraction.
 *
 * Bumping this is a breaking change for companions — pull the new
 * Ollama tag on first start; ship a new extension build before
 * flipping the default in the registry.
 */
export const SUMMARISER_MODEL_FAMILY = "qwen3-4b" as const;

export const OLLAMA_CHAT_MODEL = "qwen3:4b" as const;
export const WEBLLM_CHAT_MODEL = "Qwen3-4B-Instruct-q4f16_1-MLC" as const;

/** Canonical embedding model — 384-dim (BAAI/bge-small-en-v1.5 family).
 * Ollama ships it as `bge-small-en-v1.5`; the browser side loads the
 * same model weights via transformers.js (`Xenova/bge-small-en-v1.5`).
 * Both halves produce the SAME 384-dim vector space so a segment
 * embedded by the CLI is directly comparable to one embedded by the
 * extension — cosine similarity on cross-source segments is meaningful.
 * The wire embedding is 384-dim (BGE-small). Any future swap must keep
 * this dim invariant across both companion runtimes (CLI, browser).
 *
 * Why BGE-small over MiniLM (the earlier default): MTEB ~62 vs ~56 on
 * sentence-similarity tasks — a ~6 point gain with negligible CPU
 * penalty (33M vs 23M params). MiniLM was chosen historically because
 * Ollama's library didn't publish a bge-small tag, but it does now. */
export const OLLAMA_EMBED_MODEL = "bge-small-en-v1.5" as const;
export const BROWSER_EMBED_MODEL = "Xenova/bge-small-en-v1.5" as const;

/**
 * System prompt used by BOTH Ollama and WebLLM summarisers. Kept
 * short and strict so the local 4B model reliably returns a single
 * sentence under the 240-char output cap the pipeline enforces.
 *
 * The user message may include sampled conversation excerpts (already
 * PII-redacted by the parser + the companion's regex pass) when the
 * tool's parser supports content_excerpt. With excerpts the
 * summariser can describe what was actually being built; without
 * them it falls back to the metadata-only paraphrase.
 */
export const SUMMARISER_SYSTEM_PROMPT =
  "You summarise an AI coding session in ONE sentence, ≤ 240 characters. " +
  "If the user message includes sampled conversation excerpts, base your " +
  "summary on what the developer was actually working on (the substance — " +
  "what was being built, debugged, refactored, or designed). If only " +
  "metadata is given, paraphrase the metadata. Never quote the excerpts " +
  "verbatim. No PII, no code literals, no file paths, no API keys. " +
  "Reply with only the sentence.";

/** Hard cap on the tokens the summariser may emit. Keep well above
 * 240 chars worth so the model doesn't cut mid-word; the pipeline
 * slices to 240 chars after. */
export const SUMMARISER_MAX_TOKENS = 120;

/** Low temperature — same turns should yield the same abstract on
 * replay so our idempotency assertions in tests hold. */
export const SUMMARISER_TEMPERATURE = 0.2;

/** Approximate chars-per-token ratio for Qwen-family tokenizers.
 * Used as the cheap tokenizer fallback on both runtimes so spend
 * attribution stays directionally correct even without tiktoken. */
export const QWEN_CHARS_PER_TOKEN = 3.3;

/** Hard cap the pipeline enforces on the abstract returned by the
 * summariser. Matches the "≤240 chars" instruction baked into
 * SUMMARISER_SYSTEM_PROMPT so the model and the post-processor
 * agree on the same limit. Re-exported from pipeline/index.ts. */
export const ABSTRACT_OUTPUT_MAX_CHARS = 240;
