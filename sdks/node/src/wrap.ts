/**
 * `wrap()` — a drop-in that auto-records each LLM call so you never hand-build
 * an {@link LlmCall}.
 *
 * Wrap an `openai` or `@anthropic-ai/sdk` client and use it exactly as before;
 * every completion call is forwarded untouched, and *after* it returns the SDK
 * extracts provider / model / token usage / text and `record()`s it. Recording
 * is **best-effort**: any error in extraction or recording is swallowed so the
 * wrapped call's result and timing are never affected.
 *
 * This module is additive and dynamic — it does **not** import `openai` or
 * `@anthropic-ai/sdk` (they are optional peers). Detection is structural, by the
 * shape of the client object, so `wrap` works whether or not either SDK is
 * installed.
 *
 * ```ts
 * import OpenAI from "openai";
 * import { Client, Config, wrap } from "@modelstat/sdk";
 *
 * const ms = new Client(new Config("msk_live_…", "raw_sdk_openai"));
 * const openai = wrap(new OpenAI(), { client: ms, metadata: { feature: "search" } });
 *
 * // …unchanged usage; the call is auto-recorded…
 * const resp = await openai.chat.completions.create({
 *   model: "gpt-x",
 *   messages: [{ role: "user", content: "hello" }],
 * });
 * ```
 */

import type { Client } from "./index.js";
import { LlmCall } from "./capture.js";
import type { Metadata } from "./wire.js";

/** Options for {@link wrap}. */
export interface WrapOptions {
  /** The modelstat {@link Client} that captured calls are recorded into. */
  client: Client;
  /**
   * Wrap-default attribution tags merged into every auto-recorded call (as the
   * per-call layer, so they override `Config` defaults / ambient on shared
   * keys). `Config` defaults and the ambient {@link withMetadata} layer still
   * apply underneath.
   */
  metadata?: Metadata;
  /**
   * Grouping id for the recorded calls (the `session_id`). A constant string or
   * a function evaluated per call (e.g. to pull a request/trace id). Defaults to
   * the literal `"wrap"`.
   */
  sessionId?: string | (() => string);
}

/** A detected provider flavor for a wrapped client. */
type Provider = "openai" | "anthropic";

/**
 * Wrap a provider client so each completion call is auto-recorded. Returns a
 * transparent proxy: every property read forwards to the underlying client, and
 * the one completion method (`chat.completions.create` for OpenAI,
 * `messages.create` for Anthropic) is intercepted to record after it returns.
 *
 * Throws only if the client flavor can't be detected (a programming error);
 * everything on the live call path is otherwise non-throwing.
 */
export function wrap<T extends object>(client: T, options: WrapOptions): T {
  const provider = detectProvider(client);
  if (provider === undefined) {
    throw new Error(
      "modelstat.wrap: could not detect an OpenAI or Anthropic client " +
        "(expected `.chat.completions.create` or `.messages.create`).",
    );
  }
  return wrapObject(client, provider, options, []);
}

/**
 * Detect whether `client` looks like an OpenAI or Anthropic SDK client by the
 * presence of its completion method. Returns `undefined` when neither matches.
 */
function detectProvider(client: unknown): Provider | undefined {
  if (!isObject(client)) {
    return undefined;
  }
  // OpenAI: client.chat.completions.create
  const chat = (client as Record<string, unknown>)["chat"];
  if (isObject(chat)) {
    const completions = (chat as Record<string, unknown>)["completions"];
    if (
      isObject(completions) &&
      typeof (completions as Record<string, unknown>)["create"] === "function"
    ) {
      return "openai";
    }
  }
  // Anthropic: client.messages.create
  const messages = (client as Record<string, unknown>)["messages"];
  if (
    isObject(messages) &&
    typeof (messages as Record<string, unknown>)["create"] === "function"
  ) {
    return "anthropic";
  }
  return undefined;
}

/**
 * The dotted path to the completion method we intercept, per provider.
 */
const COMPLETION_PATH: Record<Provider, string[]> = {
  openai: ["chat", "completions", "create"],
  anthropic: ["messages", "create"],
};

/**
 * Recursively proxy `target` so that property access along the completion path
 * is preserved structurally and the terminal `create` function is intercepted.
 * `path` is the dotted location of `target` within the root client.
 */
function wrapObject<T extends object>(
  target: T,
  provider: Provider,
  options: WrapOptions,
  path: string[],
): T {
  const completionPath = COMPLETION_PATH[provider];
  return new Proxy(target, {
    get(obj, prop, receiver) {
      const value = Reflect.get(obj, prop, receiver);
      if (typeof prop === "symbol") {
        return value;
      }
      const here = [...path, prop];
      // Are we exactly at the terminal `create` method?
      if (
        typeof value === "function" &&
        pathEquals(here, completionPath)
      ) {
        return interceptCreate(obj, value as AnyFn, provider, options);
      }
      // Are we on the prefix leading to the terminal method? Keep proxying so
      // the next access stays wrapped. (`obj.chat` → proxy → `.completions` …)
      if (isObject(value) && isPrefixOf(here, completionPath)) {
        return wrapObject(value, provider, options, here);
      }
      // Bind plain methods to the real object so `this` is correct.
      if (typeof value === "function") {
        return value.bind(obj);
      }
      return value;
    },
  });
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type AnyFn = (...args: any[]) => any;

/**
 * Wrap the terminal `create` function: run the real call, then best-effort
 * record from the request args + the response. Never alters the return value or
 * lets a recording error escape.
 */
function interceptCreate(
  thisArg: object,
  create: AnyFn,
  provider: Provider,
  options: WrapOptions,
): AnyFn {
  return function wrapped(this: unknown, ...args: unknown[]): unknown {
    const request = args[0];
    // Read BEFORE the provider call. The recording runs after the response, so
    // a call built there would date itself to the moment it came back — the one
    // instant a wrapped call is not. This is the only place the start can be
    // observed, and it is what `ts` / `started_at` carry.
    const startedAt = new Date();
    const result = create.apply(thisArg, args);
    // The provider SDKs return a Promise for the non-streaming create; handle
    // both promise and (defensively) sync returns. Recording rides on a
    // `.then` so it never delays the caller's await.
    if (isPromiseLike(result)) {
      return result.then(
        (response: unknown) => {
          safeRecord(provider, options, request, response, startedAt);
          return response;
        },
        // Propagate the original rejection untouched (don't record failures).
        (err: unknown) => {
          throw err;
        },
      );
    }
    safeRecord(provider, options, request, result, startedAt);
    return result;
  };
}

/**
 * Best-effort extract + record. Wrapped in a try/catch so a malformed response
 * or a recording fault can never break the user's call.
 */
function safeRecord(
  provider: Provider,
  options: WrapOptions,
  request: unknown,
  response: unknown,
  startedAt: Date,
): void {
  try {
    const call = buildCall(provider, options, request, response, startedAt);
    options.client.record(call);
  } catch {
    // Swallow — recording is best-effort and must never surface to the caller.
  }
}

/** Build the {@link LlmCall} from the request args and the provider response. */
function buildCall(
  provider: Provider,
  options: WrapOptions,
  request: unknown,
  response: unknown,
  startedAt: Date,
): LlmCall {
  const req = isObject(request) ? (request as Record<string, unknown>) : {};
  const resp = isObject(response) ? (response as Record<string, unknown>) : {};

  const model =
    asString(resp["model"]) ?? asString(req["model"]) ?? undefined;

  const sessionId = resolveSessionId(options.sessionId);
  const call = new LlmCall(provider, sessionId);
  // The instant the request went out, measured by the interceptor. `LlmCall`'s
  // own default is construction time, which here is after the response — so
  // this overwrite is what makes the call's timing true.
  call.startedAt = startedAt;
  // `firstTokenAt` stays unset: this interceptor sees one whole response, never
  // a first chunk, so it has no such instant to state. A caller who streams and
  // times it themselves builds their own LlmCall and sets it there.
  if (model !== undefined) {
    call.model(model);
  }

  // Tokens: OpenAI uses prompt_tokens/completion_tokens; Anthropic uses
  // input_tokens/output_tokens. Read whichever the response carries.
  const usage = isObject(resp["usage"])
    ? (resp["usage"] as Record<string, unknown>)
    : {};
  const input =
    asNumber(usage["prompt_tokens"]) ?? asNumber(usage["input_tokens"]) ?? 0;
  const output =
    asNumber(usage["completion_tokens"]) ??
    asNumber(usage["output_tokens"]) ??
    0;
  call.tokens({ input, output });

  // Best-effort prompt + completion text.
  const prompt = extractPrompt(provider, req);
  const completion = extractCompletion(provider, resp);
  if (prompt !== "" || completion !== "") {
    call.text(prompt, completion);
  }

  // Wrap-default tags ride the per-call layer; ambient + Config defaults still
  // apply underneath at build time.
  if (options.metadata !== undefined) {
    call.metadata(options.metadata);
  }
  return call;
}

/** Resolve the `sessionId` option (constant, factory, or default). */
function resolveSessionId(sessionId: WrapOptions["sessionId"]): string {
  if (typeof sessionId === "function") {
    try {
      return sessionId();
    } catch {
      return "wrap";
    }
  }
  return sessionId ?? "wrap";
}

/**
 * Pull the prompt text out of a completion request. OpenAI's `messages` are
 * `{ role, content }` where content is a string or an array of parts;
 * Anthropic's are the same shape (plus a top-level `system`).
 */
function extractPrompt(
  provider: Provider,
  req: Record<string, unknown>,
): string {
  const parts: string[] = [];
  if (provider === "anthropic") {
    const system = req["system"];
    const sys = flattenContent(system);
    if (sys !== "") {
      parts.push(sys);
    }
  }
  const messages = req["messages"];
  if (Array.isArray(messages)) {
    for (const m of messages) {
      if (isObject(m)) {
        const text = flattenContent((m as Record<string, unknown>)["content"]);
        if (text !== "") {
          parts.push(text);
        }
      }
    }
  }
  return parts.join("\n");
}

/**
 * Pull the completion text out of a provider response. OpenAI:
 * `choices[].message.content`. Anthropic: `content[].text`.
 */
function extractCompletion(
  provider: Provider,
  resp: Record<string, unknown>,
): string {
  if (provider === "openai") {
    const choices = resp["choices"];
    if (Array.isArray(choices)) {
      const parts: string[] = [];
      for (const c of choices) {
        if (isObject(c)) {
          const message = (c as Record<string, unknown>)["message"];
          if (isObject(message)) {
            const text = flattenContent(
              (message as Record<string, unknown>)["content"],
            );
            if (text !== "") {
              parts.push(text);
            }
          }
        }
      }
      return parts.join("\n");
    }
    return "";
  }
  // anthropic: content is an array of blocks, text blocks carry `.text`.
  return flattenContent(resp["content"]);
}

/**
 * Flatten a message-content value into plain text. Handles a bare string, an
 * array of content parts (`{ type: "text", text }` for both providers), and
 * ignores non-text parts (images, tool-use blocks, …).
 */
function flattenContent(content: unknown): string {
  if (typeof content === "string") {
    return content;
  }
  if (Array.isArray(content)) {
    const parts: string[] = [];
    for (const part of content) {
      if (typeof part === "string") {
        parts.push(part);
      } else if (isObject(part)) {
        const text = (part as Record<string, unknown>)["text"];
        if (typeof text === "string") {
          parts.push(text);
        }
      }
    }
    return parts.join("");
  }
  return "";
}

// ---- small type guards ------------------------------------------------------

function isObject(v: unknown): v is object {
  return typeof v === "object" && v !== null;
}

function isPromiseLike(v: unknown): v is PromiseLike<unknown> {
  return isObject(v) && typeof (v as { then?: unknown }).then === "function";
}

function asString(v: unknown): string | undefined {
  return typeof v === "string" ? v : undefined;
}

function asNumber(v: unknown): number | undefined {
  return typeof v === "number" && Number.isFinite(v) ? v : undefined;
}

function pathEquals(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((x, i) => x === b[i]);
}

/** Whether `a` is a strict prefix of `b` (shorter, and matches element-wise). */
function isPrefixOf(a: string[], b: string[]): boolean {
  return a.length < b.length && a.every((x, i) => x === b[i]);
}
