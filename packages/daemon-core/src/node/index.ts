/**
 * Node-only daemon adapters. Use from the agent CLI.
 *
 * Daemons import from this subpath to get runtime-specific
 * implementations (file-backed queue store, pino logger, Ollama
 * adapters) while keeping the same contracts as the browser side.
 */
// Back-compat alias — callers that imported SqliteQueueStore still
// work without a rename. Scheduled for removal once apps/daemon
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
  llamaRedact,
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
// Remote OpenAI-compatible summariser — opt-in alternative to the bundled
// local model, selected by the daemon via MODELSTAT_LLM_PROVIDER=openai.
export {
  DEFAULT_LLM_TIMEOUT_MS,
  defaultOpenAICompatConfig,
  type OpenAICompatConfig,
  OpenAICompatConfigError,
  OpenAICompatRequestError,
  openaiCognize,
  openaiEntitle,
  openaiExtractLinks,
  openaiScriptSummarize,
  openaiSummarize,
} from "./openai-compat.js";
// Transformers.js BGE-small-en-v1.5 embedder — the wire embedding is
// 384-dim (BGE-small), so CLI and browser segment vectors land in the
// same space.
export { createTransformersJsEmbedder } from "./transformersjs-embed.js";
