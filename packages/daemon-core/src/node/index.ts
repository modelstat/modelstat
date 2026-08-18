/**
 * Node-only daemon adapters. Use from the agent CLI.
 *
 * Daemons import from this subpath to get runtime-specific
 * implementations (file-backed queue store, pino logger, Ollama
 * adapters) while keeping the same contracts as the browser side.
 */
export { FileQueueStore } from "./file-queue-store.js";
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
// On-device NER/PII redaction model — pre-warmed at `connect` so cloud/
// self-hosted (fail-closed on it) start at full quality, into a shared cache
// dir the daemon reads back.
export { ensureNerModel } from "./ner-model.js";
export {
  defaultOllamaConfig,
  type OllamaConfig,
  ollamaCognize,
  ollamaEmbed,
  ollamaSummarize,
  ollamaTokenize,
} from "./ollama.js";
// Remote OpenAI-compatible summariser — the runtime behind self-hosted mode
// (an org-run endpoint) and the env-driven remote provider.
export {
  DEFAULT_LLM_TIMEOUT_MS,
  defaultOpenAICompatConfig,
  makeOpenAICompatConfig,
  type OpenAICompatConfig,
  OpenAICompatConfigError,
  OpenAICompatRequestError,
  openaiCognize,
  openaiEntitle,
  openaiExtractLinks,
  openaiScriptSummarize,
  openaiSummarize,
  validateSummarizerUrl,
} from "./openai-compat.js";
export { applyTransformersCacheDir, transformersCacheDir } from "./transformers-cache.js";
// Transformers.js BGE-small-en-v1.5 embedder — the wire embedding is
// 384-dim (BGE-small), so CLI and browser segment vectors land in the
// same space.
export { createTransformersJsEmbedder } from "./transformersjs-embed.js";
