/**
 * Browser-only companion adapters. Use from apps/extension.
 *
 * Mirrors @modelstat/companion-core/node: same QueueStore contract,
 * IDB-backed implementation, and pipeline adapters (tokenize, embed,
 * summarize) that match the Ollama adapter shapes byte-for-byte.
 * Companion apps wire their offscreen ML runtimes here so the
 * segments the extension emits are interchangeable with segments
 * emitted by the CLI via Ollama.
 */
export { IdbQueueStore, ensureQueueStore } from "./idb-queue-store.js";
export {
  defaultWebLLMConfig,
  webllmCognize,
  webllmEmbed,
  webllmSummarize,
  webllmTokenize,
  type EmbeddingSession,
  type WebLLMChatLike,
  type WebLLMConfig,
} from "./webllm.js";
export {
  promptApiCognize,
  promptApiReady,
  promptApiSummarize,
} from "./prompt-api.js";
export { tieredSummarize, type TieredSummariserOptions } from "./summarizer.js";
export { tieredCognize, type TieredCognizerOptions } from "./cognizer.js";
