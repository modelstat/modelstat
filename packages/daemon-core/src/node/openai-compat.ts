/**
 * Remote OpenAI-compatible LLM adapters for the daemon pipeline on Node.
 *
 * A drop-in alternative to the bundled `node-llama-cpp` summariser
 * (see ./llama.ts) for machines where a local 2.7 GB model isn't an
 * option — sandboxes, CI, thin laptops. Instead of running inference
 * locally it POSTs to any `/v1/chat/completions` endpoint: OpenAI,
 * a remote Ollama (`/v1`), vLLM, LiteLLM, OpenRouter, Together, …
 *
 * Same prompt contracts as every other runtime (see ../pipeline/*),
 * so the abstract you get from a remote `gpt-4o-mini` reads like the
 * one the bundled Qwen produces. The factories mirror the llama ones:
 *   - summarize     : REQUIRED, throws on failure (the resilient wrapper
 *                     in apps/daemon degrades to the extractive fallback)
 *   - cognize/entitle/extractLinks/scriptSummarize : best-effort → null
 *
 * PRIVACY — this is an explicit egress. Unlike the local path, the
 * summariser prompt (sampled, regex-floor-redacted conversation
 * excerpts) and script bodies are sent to the configured endpoint.
 * The daemon logs a loud one-time warning when this provider is
 * selected; embeddings + the Privacy-Filter redactor stay local.
 */

import { z } from "zod";
import {
  buildCognitionUserPrompt,
  COGNITION_MAX_TOKENS,
  COGNITION_SYSTEM_PROMPT,
  COGNITION_TEMPERATURE,
  type Cognizer,
  parseCognitionReply,
} from "../pipeline/cognition.js";
import { SUMMARISER_SYSTEM_PROMPT, SUMMARISER_TEMPERATURE } from "../pipeline/prompts.js";
import {
  buildScriptSummaryUserPrompt,
  SCRIPT_SUMMARY_MAX_TOKENS,
  SCRIPT_SUMMARY_OUTPUT_MAX_CHARS,
  SCRIPT_SUMMARY_SYSTEM_PROMPT,
  SCRIPT_SUMMARY_TEMPERATURE,
  type ScriptSummarizer,
} from "../pipeline/script-summary.js";
import {
  buildLinkExtractUserPrompt,
  LINK_EXTRACT_MAX_TOKENS,
  LINK_EXTRACT_SYSTEM_PROMPT,
  LINK_EXTRACT_TEMPERATURE,
  type LinkExtractor,
} from "../pipeline/session-metadata.js";
import { stripThinking, THINKING_HEADROOM_TOKENS } from "../pipeline/thinking.js";
import {
  buildTitleUserPrompt,
  type Entitler,
  TITLER_MAX_TOKENS,
  TITLER_SYSTEM_PROMPT,
  TITLER_TEMPERATURE,
} from "../pipeline/title.js";

/**
 * Scrubs a string of PII/secrets BEFORE it is POSTed to the remote
 * endpoint. The remote path is an explicit egress, so raw prompt content
 * (conversation excerpts, script file bodies) must pass an on-device NER
 * pass first — the local path skips this because nothing leaves the box.
 * Supplied by the daemon (apps/daemon/src/pipeline.ts); when omitted the
 * content is sent as-is (used by the wire-level tests).
 */
export type PreSendRedactor = (text: string) => Promise<string>;

export interface OpenAICompatConfig {
  /** Base URL up to and including the API version, e.g.
   * `https://api.openai.com/v1` or `http://localhost:11434/v1`.
   * `/chat/completions` is appended. */
  readonly baseUrl: string;
  /** Bearer token. Null for endpoints that don't require auth (local
   * Ollama / vLLM). */
  readonly apiKey: string | null;
  /** Model identifier as the endpoint names it, e.g. `gpt-4o-mini`,
   * `qwen2.5:4b`, `meta-llama/Llama-3.1-8B-Instruct`. */
  readonly model: string;
  /** Per-request timeout. Remote endpoints are less reliable than a
   * localhost model, so every call is bounded. */
  readonly timeoutMs: number;
}

/** Raised when the remote provider is selected but its required env is
 * missing — surfaces at daemon startup, not mid-scan. */
export class OpenAICompatConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "OpenAICompatConfigError";
  }
}

/** A non-2xx response or a transport failure from the endpoint. `status`
 * is 0 for transport-level failures (DNS, timeout, refused). */
export class OpenAICompatRequestError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "OpenAICompatRequestError";
    this.status = status;
  }
}

export const DEFAULT_LLM_TIMEOUT_MS = 60_000;

/** Bounded per-call retry for the remote endpoint. Unlike the local
 * model (which fails hard or not at all), an HTTP endpoint has transient
 * failures — a brief 5xx, a rate-limit, a network blip — that a short
 * retry rides through. Kept small so a hard-down endpoint still surfaces
 * quickly to the resilient wrapper's fallback rather than stalling a
 * scan. Backoff is exponential (500ms, 1s, 2s …) capped, and a 429's
 * `Retry-After` header wins when present. */
const MAX_RETRY_ATTEMPTS = 3;
const BASE_BACKOFF_MS = 500;
const MAX_BACKOFF_MS = 8_000;

const delay = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/** Token budget for the summarise call. Generous so a thinking model
 * doesn't starve its reasoning pass; non-thinking models stop at EOS
 * well before this. The sentence cap is enforced by the slice, not the
 * budget. */
const SUMMARY_MAX_TOKENS = 1024;

/**
 * Resolve the remote-LLM config from the environment. Throws
 * {@link OpenAICompatConfigError} when `MODELSTAT_LLM_BASE_URL` or
 * `MODELSTAT_LLM_MODEL` is absent — fail-fast at the boundary rather
 * than ship empty abstracts.
 */
export function defaultOpenAICompatConfig(): OpenAICompatConfig {
  const env = process.env;
  const baseUrl = env.MODELSTAT_LLM_BASE_URL?.trim();
  const model = env.MODELSTAT_LLM_MODEL?.trim();
  if (!baseUrl) {
    throw new OpenAICompatConfigError(
      'MODELSTAT_LLM_BASE_URL is required when MODELSTAT_LLM_PROVIDER=openai (e.g. "https://api.openai.com/v1").',
    );
  }
  if (!model) {
    throw new OpenAICompatConfigError(
      'MODELSTAT_LLM_MODEL is required when MODELSTAT_LLM_PROVIDER=openai (e.g. "gpt-4o-mini").',
    );
  }
  const timeoutRaw = Number(env.MODELSTAT_LLM_TIMEOUT_MS ?? DEFAULT_LLM_TIMEOUT_MS);
  return {
    baseUrl,
    apiKey: env.MODELSTAT_LLM_API_KEY?.trim() || null,
    model,
    timeoutMs: Number.isFinite(timeoutRaw) && timeoutRaw > 0 ? timeoutRaw : DEFAULT_LLM_TIMEOUT_MS,
  };
}

/** Minimal shape of the OpenAI chat-completions response we depend on.
 * Parsed (not cast) because it crosses a trust boundary. */
const ChatCompletionResponse = z.object({
  choices: z
    .array(z.object({ message: z.object({ content: z.string().nullish() }).optional() }))
    .default([]),
});

interface ChatRequest {
  readonly system: string;
  readonly user: string;
  readonly temperature: number;
  readonly maxTokens: number;
}

/** Reasoning models (OpenAI o-series, gpt-5) reject the classic
 * `max_tokens` field — they require `max_completion_tokens` — and reject
 * any non-default `temperature`. Detect them by name so the request body
 * matches the endpoint's contract instead of 400-ing on every call. */
function isReasoningModel(model: string): boolean {
  return /(^|[/:])(o[1-9](-|$)|gpt-5)/i.test(model.trim());
}

/** Build the provider-correct chat-completions body. Reasoning models
 * get `max_completion_tokens` and no `temperature`; everything else
 * keeps the widely-supported `max_tokens` + `temperature` pair. */
function buildChatBody(cfg: OpenAICompatConfig, req: ChatRequest, user: string): string {
  const messages = [
    { role: "system", content: req.system },
    { role: "user", content: user },
  ];
  const base = { model: cfg.model, stream: false as const, messages };
  return JSON.stringify(
    isReasoningModel(cfg.model)
      ? { ...base, max_completion_tokens: req.maxTokens }
      : { ...base, temperature: req.temperature, max_tokens: req.maxTokens },
  );
}

/** A 429's `Retry-After` (seconds) as ms, when the header is a sane
 * non-negative number; otherwise null so the caller uses backoff. */
function retryAfterMs(res: Response): number | null {
  const header = res.headers.get("retry-after");
  if (!header) return null;
  const secs = Number(header);
  return Number.isFinite(secs) && secs >= 0 ? Math.min(secs * 1000, MAX_BACKOFF_MS) : null;
}

function transportError(cfg: OpenAICompatConfig, err: unknown): OpenAICompatRequestError {
  if (err instanceof DOMException && err.name === "TimeoutError") {
    return new OpenAICompatRequestError(
      0,
      `request to ${cfg.baseUrl} timed out after ${cfg.timeoutMs}ms`,
    );
  }
  const reason = err instanceof Error ? err.message : String(err);
  return new OpenAICompatRequestError(0, `request to ${cfg.baseUrl} failed: ${reason}`);
}

async function httpError(res: Response): Promise<OpenAICompatRequestError> {
  const body = await res.text().catch(() => "");
  return new OpenAICompatRequestError(
    res.status,
    `chat completion failed: ${res.status} ${body.slice(0, 200)}`,
  );
}

/** Parse a 2xx reply into the answer text, wrapping a non-JSON body or
 * an unexpected shape as {@link OpenAICompatRequestError} so callers see
 * one error type instead of a raw `SyntaxError`/`ZodError`. */
async function parseReply(cfg: OpenAICompatConfig, res: Response): Promise<string> {
  let json: unknown;
  try {
    json = await res.json();
  } catch {
    throw new OpenAICompatRequestError(res.status, `non-JSON response from ${cfg.baseUrl}`);
  }
  const parsed = ChatCompletionResponse.safeParse(json);
  if (!parsed.success) {
    throw new OpenAICompatRequestError(
      res.status,
      `unexpected response schema from ${cfg.baseUrl}`,
    );
  }
  return stripThinking(parsed.data.choices[0]?.message?.content ?? "");
}

/** One non-streaming chat completion, with bounded retry. `preSend`
 * scrubs the user content (PII/secrets) before it leaves the machine.
 * Retries transport failures, timeouts, 429s, and 5xxs with exponential
 * backoff (honouring `Retry-After`); 4xx client errors are returned
 * immediately. Returns the assistant message with any `<think>` block
 * stripped. Throws {@link OpenAICompatRequestError} once retries are
 * exhausted. */
async function chatComplete(
  cfg: OpenAICompatConfig,
  req: ChatRequest,
  preSend?: PreSendRedactor,
): Promise<string> {
  const user = preSend ? await preSend(req.user) : req.user;
  const url = `${cfg.baseUrl.replace(/\/+$/, "")}/chat/completions`;
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (cfg.apiKey) headers.authorization = `Bearer ${cfg.apiKey}`;
  const body = buildChatBody(cfg, req, user);

  let lastErr = new OpenAICompatRequestError(0, "request not attempted");
  for (let attempt = 0; attempt < MAX_RETRY_ATTEMPTS; attempt++) {
    const isLast = attempt === MAX_RETRY_ATTEMPTS - 1;
    let res: Response;
    try {
      res = await fetch(url, {
        method: "POST",
        headers,
        body,
        signal: AbortSignal.timeout(cfg.timeoutMs),
      });
    } catch (err) {
      lastErr = transportError(cfg, err);
      if (!isLast) await delay(Math.min(BASE_BACKOFF_MS * 2 ** attempt, MAX_BACKOFF_MS));
      continue;
    }
    // Retryable server-side conditions: rate limit + transient 5xx.
    if (res.status === 429 || res.status >= 500) {
      lastErr = await httpError(res);
      if (!isLast) {
        const wait = retryAfterMs(res) ?? Math.min(BASE_BACKOFF_MS * 2 ** attempt, MAX_BACKOFF_MS);
        await delay(wait);
      }
      continue;
    }
    // 4xx (bad model name, auth, malformed request) won't fix on retry.
    if (!res.ok) throw await httpError(res);
    return await parseReply(cfg, res);
  }
  throw lastErr;
}

/**
 * Summariser adapter — drop-in for `llamaSummarize`/`ollamaSummarize`.
 * Throws on transport failure or empty output so the resilient wrapper
 * in apps/daemon can degrade to the extractive fallback.
 *
 * `preSend` runs the on-device NER/PII pass over the prompt (which
 * embeds the sampled conversation excerpts) BEFORE it is POSTed — the
 * remote path's egress guard. The local llama path needs no equivalent
 * because the prompt never leaves the machine.
 */
export function openaiSummarize(
  cfg: OpenAICompatConfig,
  preSend?: PreSendRedactor,
): (input: { prompt: string; maxTokens: number }) => Promise<string> {
  return async ({ prompt }) => {
    const out = await chatComplete(
      cfg,
      {
        system: SUMMARISER_SYSTEM_PROMPT,
        user: prompt,
        temperature: SUMMARISER_TEMPERATURE,
        maxTokens: SUMMARY_MAX_TOKENS,
      },
      preSend,
    );
    if (out.length === 0) {
      throw new OpenAICompatRequestError(
        0,
        `remote summariser (${cfg.model}) returned no answer text after stripping <think> blocks`,
      );
    }
    return out.slice(0, 240);
  };
}

/** Cognition adapter — best-effort mood/mind/posture tags from the
 * already-redacted abstract. Any failure ⇒ null (segment ships without
 * the suffix). Mirrors `llamaCognize`. */
export function openaiCognize(cfg: OpenAICompatConfig): Cognizer {
  return async ({ abstract }) => {
    if (!abstract || abstract.trim().length < 12) return null;
    try {
      const out = await chatComplete(cfg, {
        system: COGNITION_SYSTEM_PROMPT,
        user: buildCognitionUserPrompt(abstract),
        temperature: COGNITION_TEMPERATURE,
        maxTokens: COGNITION_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      return parseCognitionReply(out);
    } catch {
      // best-effort by contract: any transport/parse failure ⇒ no signal.
      return null;
    }
  };
}

/** Session-title adapter — best-effort, one short call per session over
 * the redacted abstracts. Failure ⇒ null and the caller falls back to a
 * deterministic title. Mirrors `llamaEntitle`. */
export function openaiEntitle(cfg: OpenAICompatConfig): Entitler {
  return async (input) => {
    if (input.abstracts.length === 0) return null;
    try {
      const out = await chatComplete(cfg, {
        system: TITLER_SYSTEM_PROMPT,
        user: buildTitleUserPrompt(input),
        temperature: TITLER_TEMPERATURE,
        maxTokens: TITLER_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      return out || null;
    } catch {
      // best-effort by contract: failure ⇒ deterministic fallback title.
      return null;
    }
  };
}

/** Link-extraction adapter — best-effort, surfaces PR/issue/commit refs
 * from the redacted abstracts as free text the caller re-parses
 * deterministically. Failure ⇒ null. Mirrors `llamaExtractLinks`. */
export function openaiExtractLinks(cfg: OpenAICompatConfig): LinkExtractor {
  return async ({ abstracts }) => {
    if (abstracts.length === 0) return null;
    try {
      const out = await chatComplete(cfg, {
        system: LINK_EXTRACT_SYSTEM_PROMPT,
        user: buildLinkExtractUserPrompt(abstracts),
        temperature: LINK_EXTRACT_TEMPERATURE,
        maxTokens: LINK_EXTRACT_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      return out || null;
    } catch {
      // best-effort by contract: failure ⇒ deterministic git/content channels.
      return null;
    }
  };
}

/** Per-script content-summary adapter — best-effort. The agent reads
 * the file and redacts the returned sentence; failure ⇒ null and the
 * call ships without a script abstract. Mirrors `llamaScriptSummarize`. */
export function openaiScriptSummarize(
  cfg: OpenAICompatConfig,
  preSend?: PreSendRedactor,
): ScriptSummarizer {
  return async ({ ref, content }) => {
    if (!content || content.trim().length === 0) return null;
    try {
      const out = await chatComplete(
        cfg,
        {
          system: SCRIPT_SUMMARY_SYSTEM_PROMPT,
          user: buildScriptSummaryUserPrompt({ ref, content }),
          temperature: SCRIPT_SUMMARY_TEMPERATURE,
          maxTokens: SCRIPT_SUMMARY_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
        },
        preSend,
      );
      const oneLine = out.replace(/\s+/g, " ").trim();
      return oneLine ? oneLine.slice(0, SCRIPT_SUMMARY_OUTPUT_MAX_CHARS) : null;
    } catch {
      // best-effort by contract: failure ⇒ ship the call without a script abstract.
      return null;
    }
  };
}
