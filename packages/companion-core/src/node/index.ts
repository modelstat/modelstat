/**
 * Node-only companion adapters. Use from the agent CLI.
 *
 * Companions import from this subpath to get runtime-specific
 * implementations (file-backed queue store, pino logger, Ollama
 * adapters) while keeping the same contracts as the browser side.
 */
export { FileQueueStore } from "./file-queue-store.js";
// Back-compat alias — callers that imported SqliteQueueStore still
// work without a rename. Scheduled for removal once apps/agent-dev
// ships a release using FileQueueStore directly.
export { FileQueueStore as SqliteQueueStore } from "./file-queue-store.js";
export {
  defaultOllamaConfig,
  ollamaCognize,
  ollamaEmbed,
  ollamaSummarize,
  ollamaTokenize,
  type OllamaConfig,
} from "./ollama.js";
export {
  defaultLlamaConfig,
  ensureLlamaModel,
  llamaCognize,
  llamaEntitle,
  llamaSummarize,
  DEFAULT_LLAMA_MODEL_URL,
  type LlamaConfig,
} from "./llama.js";
// Transformers.js BGE-small-en-v1.5 embedder — the wire embedding is
// 384-dim (BGE-small), so CLI and browser segment vectors land in the
// same space.
export { createTransformersJsEmbedder } from "./transformersjs-embed.js";
