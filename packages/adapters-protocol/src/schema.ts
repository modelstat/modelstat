/**
 * Declarative adapter configuration — the wire format between the
 * modelstat API (publisher) and the Chrome extension (interpreter).
 *
 * Design invariant: configs are PURE DATA. The interpreter implements a
 * fixed, audited capability surface; it never evals, never loads
 * additional URLs specified by the config, never treats strings as
 * function pointers. Adding a new extractor kind requires shipping a
 * new extension build — on purpose. This is what keeps us safely inside
 * Chrome Web Store policy on remote code.
 *
 * Every extractor is a pure function (DOM / URL / network frame) → value.
 * Every field on a message carries an ORDERED list of extractor variants
 * plus a merge strategy ("coalesce" = first non-null; "merge" =
 * field-level first-non-null). One variant working is enough. Adding
 * more only increases reliability.
 */
import { z } from "zod";
import { AGENTS, PROVIDERS } from "@modelstat/core/enums";
import { ADAPTER_PROTOCOL_VERSION } from "./version.js";

// ──────────────────────────────────────────────────────────────────────
// Primitives — tiny, auditable extractor kinds. Adding new kinds
// requires an extension release; configs cannot invent new ones.
// ──────────────────────────────────────────────────────────────────────

/** Extract a named capture group from a regex matched against the
 * current page URL. */
export const UrlRegexGroupExtractor = z.object({
  kind: z.literal("url.regexGroup"),
  pattern: z.string().max(400),
  /** Group index (1+) or named group. Defaults to 1. */
  group: z.union([z.number().int().positive(), z.string().max(60)]).default(1),
});

/** Query the DOM, return `.textContent` (trimmed). */
export const DomSelectorTextExtractor = z.object({
  kind: z.literal("dom.selector.text"),
  selector: z.string().max(400),
  /** Optional regex applied to textContent; if provided, return capture
   * group `group` (default 1). */
  regex: z.string().max(200).optional(),
  group: z.union([z.number().int().nonnegative(), z.string().max(60)]).default(1),
});

/** Query the DOM, return an attribute value. */
export const DomSelectorAttrExtractor = z.object({
  kind: z.literal("dom.selector.attr"),
  selector: z.string().max(400),
  attr: z.string().max(80),
});

/** Query the DOM, return a dataset key (`data-*` attribute). */
export const DomSelectorDatasetExtractor = z.object({
  kind: z.literal("dom.selector.dataset"),
  selector: z.string().max(400),
  /** camelCase key (matches `HTMLElement.dataset` API). */
  key: z.string().max(80),
});

/** Observe elements matching `selector` via MutationObserver. Each
 * matched element becomes a "message candidate": the interpreter pulls
 * `idAttr`, `roleAttr`, and `textSelector.textContent` and emits a
 * message event. */
export const DomObserveExtractor = z.object({
  kind: z.literal("dom.observe"),
  selector: z.string().max(400),
  /** DOM attribute carrying a stable message id (required for dedupe). */
  idAttr: z.string().max(80),
  /** DOM attribute carrying the role ("user" / "assistant"). If absent,
   * falls back to `roleDefault`. */
  roleAttr: z.string().max(80).optional(),
  roleDefault: z.enum(["user", "assistant"]).optional(),
  /** Nested selector whose `.textContent` becomes the message text. */
  textSelector: z.string().max(400).optional(),
});

/** Minimum-match invariant for canary checks. */
export const DomMinCountInvariant = z.object({
  kind: z.literal("dom.minCount"),
  selector: z.string().max(400),
  min: z.number().int().positive().max(10_000),
});

/** Intercept a fetch/XHR response whose URL matches `urlPattern` (regex)
 * and extract fields via JSONPath expressions. For SSE, set `sse: true`
 * — each event line is parsed as JSON, paths run per-frame. */
export const NetworkResponseJsonPathExtractor = z.object({
  kind: z.literal("network.responseJsonPath"),
  /** Regex matched against the full URL. */
  urlPattern: z.string().max(400),
  /** Optional HTTP method filter. */
  method: z.enum(["GET", "POST", "PUT", "PATCH"]).optional(),
  /** True → body is parsed as Server-Sent Events; each `data:` frame
   * becomes one JSON document. */
  sse: z.boolean().default(false),
  /** Single-value JSONPath for scalar extractors (model, conversation_id). */
  path: z.string().max(400).optional(),
  /** Per-message extractors for the "messages" field — each SSE frame or
   * JSON body may yield one or more messages. */
  messageIdPath: z.string().max(400).optional(),
  rolePath: z.string().max(400).optional(),
  textPath: z.string().max(400).optional(),
  usagePath: z.string().max(400).optional(),
  /** Optional map: JSONPath on usage object → TokenUsage sub-field. */
  usageFields: z
    .object({
      input: z.string().max(200).optional(),
      output: z.string().max(200).optional(),
      cache_creation: z.string().max(200).optional(),
      cache_read: z.string().max(200).optional(),
      reasoning: z.string().max(200).optional(),
    })
    .optional(),
});

/** Like `network.responseJsonPath` but against the REQUEST body — used
 * when the client sends the model in the outgoing request (many chat
 * UIs do). */
export const NetworkRequestJsonPathExtractor = z.object({
  kind: z.literal("network.requestJsonPath"),
  urlPattern: z.string().max(400),
  method: z.enum(["POST", "PUT", "PATCH"]).default("POST"),
  path: z.string().max(400),
});

// ──────────────────────────────────────────────────────────────────────
// Variant unions per field
// ──────────────────────────────────────────────────────────────────────

// Inferred types per extractor — exported so consumers can write
// narrowed types like `NetworkResponseJsonPathExtractor` instead of
// discriminating on the union every time.
export type UrlRegexGroupExtractor = z.infer<typeof UrlRegexGroupExtractor>;
export type DomSelectorTextExtractor = z.infer<typeof DomSelectorTextExtractor>;
export type DomSelectorAttrExtractor = z.infer<typeof DomSelectorAttrExtractor>;
export type DomSelectorDatasetExtractor = z.infer<typeof DomSelectorDatasetExtractor>;
export type DomObserveExtractor = z.infer<typeof DomObserveExtractor>;
export type DomMinCountInvariant = z.infer<typeof DomMinCountInvariant>;
export type NetworkResponseJsonPathExtractor = z.infer<typeof NetworkResponseJsonPathExtractor>;
export type NetworkRequestJsonPathExtractor = z.infer<typeof NetworkRequestJsonPathExtractor>;

export const ScalarExtractor = z.discriminatedUnion("kind", [
  UrlRegexGroupExtractor,
  DomSelectorTextExtractor,
  DomSelectorAttrExtractor,
  DomSelectorDatasetExtractor,
  NetworkResponseJsonPathExtractor,
  NetworkRequestJsonPathExtractor,
]);
export type ScalarExtractor = z.infer<typeof ScalarExtractor>;

export const MessagesExtractor = z.discriminatedUnion("kind", [
  DomObserveExtractor,
  NetworkResponseJsonPathExtractor,
]);
export type MessagesExtractor = z.infer<typeof MessagesExtractor>;

export const Invariant = z.discriminatedUnion("kind", [DomMinCountInvariant]);
export type Invariant = z.infer<typeof Invariant>;

// ──────────────────────────────────────────────────────────────────────
// Tokenizer binding — names MUST exist in the extension's bundled
// tokenizer registry. The interpreter rejects unknown names.
// ──────────────────────────────────────────────────────────────────────

export const TokenizerBinding = z.object({
  /** Default tokenizer name if no per-model match. */
  default: z.string().max(80),
  /** Glob-ish map: model-name pattern (with `*`) → tokenizer name.
   * Matched in declaration order; first match wins. */
  byModel: z.record(z.string().max(120), z.string().max(80)).optional(),
});

// ──────────────────────────────────────────────────────────────────────
// Top-level AdapterConfig
// ──────────────────────────────────────────────────────────────────────

export const AdapterConfig = z.object({
  protocol_version: z.literal(ADAPTER_PROTOCOL_VERSION),
  /** modelstat `AGENTS` enum value — tags emitted RawEvents. */
  provider: z.enum(AGENTS),
  /** Default `PROVIDERS` enum for emitted RawEvents. */
  vendor: z.enum(PROVIDERS),
  /** Adapter authoring version — bumped on every published change. */
  adapter_version: z.number().int().nonnegative(),
  /** chrome `host_permissions`-style match patterns. */
  match: z.array(z.string().max(400)).min(1).max(8),
  /** Fallback model name when no extractor yields one. */
  default_model: z.string().max(120).nullable().default(null),
  /** Ordered lists of extractor variants per field. First success wins
   * for scalar fields ("coalesce"). For `messages`, every variant runs
   * and results are merged per (message_id, role) with first-non-null
   * per sub-field. */
  extractors: z.object({
    conversation_id: z.array(ScalarExtractor).default([]),
    model: z.array(ScalarExtractor).default([]),
    messages: z.array(MessagesExtractor).default([]),
  }),
  tokenizer: TokenizerBinding,
  invariants: z.array(Invariant).default([]),
});
export type AdapterConfig = z.infer<typeof AdapterConfig>;

// ──────────────────────────────────────────────────────────────────────
// Adapter delivery manifest — what the API serves
// ──────────────────────────────────────────────────────────────────────

export const ManifestEntry = z.object({
  url: z.string().max(400),
  sha256: z.string().length(64),
  adapter_version: z.number().int().nonnegative(),
  signed_at: z.string().datetime({ offset: true }),
});
export type ManifestEntry = z.infer<typeof ManifestEntry>;

export const AdapterManifest = z.object({
  protocol_version: z.literal(ADAPTER_PROTOCOL_VERSION),
  generated_at: z.string().datetime({ offset: true }),
  adapters: z.record(z.enum(AGENTS), ManifestEntry),
});
export type AdapterManifest = z.infer<typeof AdapterManifest>;
