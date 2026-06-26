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
import type { Cognizer } from "../pipeline/cognition.js";
import type { Summarizer } from "../pipeline/index.js";
import {
  type ChatComplete,
  type ChatRequest,
  makeCognizer,
  makeEntitler,
  makeLinkExtractor,
  makeScriptSummarizer,
  makeSummarizer,
} from "../pipeline/passes.js";
import type { ScriptSummarizer } from "../pipeline/script-summary.js";
import type { LinkExtractor } from "../pipeline/session-metadata.js";
import { stripThinking } from "../pipeline/thinking.js";
import type { Entitler } from "../pipeline/title.js";

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
/** Floor on any retry wait so a `Retry-After: 0` (or a tiny value) can't
 * fire all attempts back-to-back in a microsecond burst against a
 * rate-limited endpoint. */
const MIN_RETRY_WAIT_MS = 250;

const delay = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

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
  // Validate the URL up front: a typo or a non-http(s) scheme should surface
  // as a clear config error (which the daemon degrades on) rather than a
  // mid-scan transport failure — and rejecting non-http(s) keeps the operator
  // from accidentally pointing the egress at a file:// or other scheme.
  let parsedUrl: URL;
  try {
    parsedUrl = new URL(baseUrl);
  } catch {
    throw new OpenAICompatConfigError(`MODELSTAT_LLM_BASE_URL is not a valid URL: "${baseUrl}".`);
  }
  if (parsedUrl.protocol !== "http:" && parsedUrl.protocol !== "https:") {
    throw new OpenAICompatConfigError(
      `MODELSTAT_LLM_BASE_URL must use http(s): "${baseUrl}" (got "${parsedUrl.protocol}").`,
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

/** Reasoning models (OpenAI o-series, gpt-5) reject the classic
 * `max_tokens` field — they require `max_completion_tokens` — and reject
 * any non-default `temperature`. Detect them by name so the request body
 * matches the endpoint's contract instead of 400-ing on every call.
 * `o\d+` covers o1…o9 and future two-digit o-series (o10+); `gpt-5(-|$)`
 * matches gpt-5 and gpt-5-* without false-matching gpt-50. The optional
 * `[/:]`-prefix allows vendor-namespaced ids like `openai/o3`. */
function isReasoningModel(model: string): boolean {
  return /(^|[/:])(o\d+(-|$)|gpt-5(-|$))/i.test(model.trim());
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
  if (!Number.isFinite(secs) || secs < 0) return null;
  // Clamp into [MIN_RETRY_WAIT_MS, MAX_BACKOFF_MS]: a 0 must still pause
  // briefly, and a multi-minute quota window is capped so a hard-down
  // endpoint surfaces to the extractive fallback quickly.
  return Math.min(Math.max(secs * 1000, MIN_RETRY_WAIT_MS), MAX_BACKOFF_MS);
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

function httpError(res: Response): OpenAICompatRequestError {
  // Status + reason phrase ONLY — deliberately NOT the response body. On a
  // content-policy rejection some endpoints echo the offending prompt
  // fragment in the body, and this message propagates to the local daemon
  // log via `markDegraded`. The status (e.g. 401/404/429) is enough to
  // diagnose a misconfigured endpoint without risking a redacted-PII echo.
  const reason = res.statusText ? ` ${res.statusText}` : "";
  return new OpenAICompatRequestError(res.status, `chat completion failed: ${res.status}${reason}`);
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
      lastErr = httpError(res);
      if (!isLast) {
        const wait = retryAfterMs(res) ?? Math.min(BASE_BACKOFF_MS * 2 ** attempt, MAX_BACKOFF_MS);
        await delay(wait);
      }
      continue;
    }
    // 4xx (bad model name, auth, malformed request) won't fix on retry.
    if (!res.ok) throw httpError(res);
    return await parseReply(cfg, res);
  }
  throw lastErr;
}

/**
 * A {@link ChatComplete} backed by this endpoint — the remote runtime's
 * single point of variation; the per-pass logic lives in
 * `../pipeline/passes.ts`. `preSend` scrubs the user content (PII/secrets)
 * before it leaves the machine: pass it for passes that carry RAW content
 * (summarise, script-summary) and OMIT it for passes over already-redacted
 * abstracts (cognition, title, link-extract) so we never re-run NER on
 * clean input.
 */
export function openaiChat(cfg: OpenAICompatConfig, preSend?: PreSendRedactor): ChatComplete {
  return (req) => chatComplete(cfg, req, preSend);
}

/**
 * Summariser adapter — REQUIRED output, drop-in for `llamaSummarize`.
 * Throws on transport failure or empty output so the resilient wrapper in
 * apps/daemon can degrade to the extractive fallback. `preSend` is the
 * remote path's egress guard: the prompt (which embeds the sampled
 * conversation excerpts) is NER/PII-scrubbed on-device before it is
 * POSTed — the local llama path needs no equivalent because the prompt
 * never leaves the machine.
 */
export function openaiSummarize(cfg: OpenAICompatConfig, preSend?: PreSendRedactor): Summarizer {
  return makeSummarizer(openaiChat(cfg, preSend));
}

/** Cognition adapter — best-effort mood/mind/posture tags over the
 * already-redacted abstract (no `preSend`). Mirrors `llamaCognize`. */
export function openaiCognize(cfg: OpenAICompatConfig): Cognizer {
  return makeCognizer(openaiChat(cfg));
}

/** Session-title adapter — best-effort, over already-redacted abstracts.
 * Mirrors `llamaEntitle`. */
export function openaiEntitle(cfg: OpenAICompatConfig): Entitler {
  return makeEntitler(openaiChat(cfg));
}

/** Link-extraction adapter — best-effort, over already-redacted
 * abstracts. Mirrors `llamaExtractLinks`. */
export function openaiExtractLinks(cfg: OpenAICompatConfig): LinkExtractor {
  return makeLinkExtractor(openaiChat(cfg));
}

/** Per-script content-summary adapter — best-effort. `preSend` scrubs the
 * raw script body before it is POSTed (remote egress guard). Mirrors
 * `llamaScriptSummarize`. */
export function openaiScriptSummarize(
  cfg: OpenAICompatConfig,
  preSend?: PreSendRedactor,
): ScriptSummarizer {
  return makeScriptSummarizer(openaiChat(cfg, preSend));
}
