/**
 * Node-only companion adapters. Use from the agent CLI.
 *
 * Companions import from this subpath to get runtime-specific
 * implementations (file-backed queue store, pino logger, Ollama
 * adapters) while keeping the same contracts as the browser side.
 */
// Back-compat alias — callers that imported SqliteQueueStore still
// work without a rename. Scheduled for removal once apps/agent-dev
// ships a release using FileQueueStore directly.
export { FileQueueStore, FileQueueStore as SqliteQueueStore } from "./file-queue-store.js";
export {
  DEFAULT_LLAMA_MODEL_URL,
  defaultLlamaConfig,
  disposeLlama,
  ensureLlamaModel,
  type LlamaConfig,
  llamaCognize,
  llamaEntitle,
  llamaExtractLinks,
  llamaScriptSummarize,
  llamaSummarize,
} from "./llama.js";
export {
  defaultOllamaConfig,
  type OllamaConfig,
  ollamaCognize,
  ollamaEmbed,
  ollamaSummarize,
  ollamaTokenize,
} from "./ollama.js";
// Transformers.js BGE-small-en-v1.5 embedder — the wire embedding is
// 384-dim (BGE-small), so CLI and browser segment vectors land in the
// same space.
export { createTransformersJsEmbedder } from "./transformersjs-embed.js";
