/**
 * The capture surface: what a caller hands the SDK per LLM call, and the
 * (worker-side) conversion into wire records.
 *
 * Building an {@link LlmCall} and calling `Client.record` is the only thing
 * that happens on the live request path — it must stay a cheap push into a
 * buffer. All of the work here (redaction, hashing, id derivation) runs later,
 * on the background worker, off the hot path.
 */

import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex, utf8ToBytes } from "@noble/hashes/utils.js";
import type { Config } from "./config.js";
import { ambientMetadata } from "./context.js";
import { redact } from "./redact.js";
import {
  batchId,
  capMetadata,
  contentHash,
  sourceEventId,
  zeroTokens,
  type Metadata,
  type EventKind,
  type GitContext,
  type IngestBatch,
  type RawEvent,
  type TokenUsage,
  type ToolCallStatus,
  type ToolCallWire,
} from "./wire.js";

/** The excerpt cap for the standard (non-raw) path, in Unicode code points. */
const EXCERPT_MAX_CHARS = 320;

/**
 * One captured tool invocation. The SDK is in the call path, so it has the
 * real args and result — it hashes/sizes them here (never ships them raw).
 */
export interface ToolCallInput {
  /** Bare tool name (`Bash`, `create_pr`). */
  name: string;
  /** `builtin` or `mcp:<server>`. Defaults to `builtin`. */
  server?: string;
  /** The call's arguments, if any. Hashed and sized; never shipped. */
  args?: unknown;
  /** Byte length of the result/output (the SDK sizes it; never ships it). */
  resultBytes?: number;
  status: ToolCallStatus;
  /** Defaults to now. */
  startedAt?: Date;
  endedAt?: Date;
  /** Allowlisted command verbs for shell-ish tools (≤3, each short). */
  commandFamilies?: string[];
}

/**
 * One captured LLM call. Construct with `new LlmCall(provider, sessionId)` and
 * either set the public fields directly or chain the fluent builders
 * `.model()`, `.tokens()`, `.text()`. `prompt`/`completion` are raw here and
 * are redacted on the worker.
 *
 * The model and token usage are exposed through the fluent builder *methods*
 * `.model(...)` / `.tokens(...)` (returning `this`); their backing values live
 * in the public `modelName` / `tokenUsage` fields, which you may also set or
 * read directly. (TypeScript can't share one name between a method and a data
 * field, so the Rust reference's `pub model` field + `.model()` builder is
 * split into a field with a distinct name plus a same-named builder.)
 */
export class LlmCall {
  provider: string;
  /** The model name, if known. Set via `.model(...)` or assigned directly. */
  modelName?: string;
  /** Trace/conversation id used to group calls into a session downstream. */
  sessionId: string;
  kind: EventKind = "assistant_message";
  /** Token usage. Set via `.tokens(...)` or assigned directly. */
  tokenUsage: TokenUsage = zeroTokens();
  startedAt: Date = new Date();
  durationMs?: number;
  prompt?: string;
  completion?: string;
  cwd?: string;
  git?: GitContext;
  toolCalls: ToolCallInput[] = [];
  /**
   * Per-call attribution tags. The highest-priority layer: these override the
   * ambient context layer and `Config` defaults on shared keys. Set via
   * `.metadata({...})` (which merges) or assigned directly. Capped before send.
   */
  metadataTags: Metadata = {};
  /**
   * Snapshot of the ambient ({@link withMetadata}) tags captured on the hot
   * path at `record()` time — *not* set by callers. The worker fills this in
   * because the merge runs later, off the `withMetadata` scope. The middle
   * layer: above `Config` defaults, below {@link LlmCall.metadataTags}.
   */
  ambientMetadataSnapshot?: Metadata;

  /** A call with `startedAt = now` and `kind = "assistant_message"`. */
  constructor(provider: string, sessionId: string) {
    this.provider = provider;
    this.sessionId = sessionId;
  }

  /** Fluent: set the model. Returns `this` for chaining. */
  model(model: string): this {
    this.modelName = model;
    return this;
  }

  /**
   * Fluent: set token usage (partial — any classes you omit default to zero).
   * Returns `this` for chaining.
   */
  tokens(tokens: Partial<TokenUsage>): this {
    this.tokenUsage = { ...zeroTokens(), ...tokens };
    return this;
  }

  /** Fluent: set the prompt and completion text (raw; redacted on the worker). */
  text(prompt: string, completion: string): this {
    this.prompt = prompt;
    this.completion = completion;
    return this;
  }

  /**
   * Fluent: merge per-call attribution tags (each key overwrites any previous
   * value, including the same-keyed default/ambient tag). Returns `this` for
   * chaining.
   *
   * The backing field is `metadataTags` (TypeScript can't share one name
   * between this builder method and a data field), which you may also set or
   * read directly.
   */
  metadata(tags: Metadata): this {
    this.metadataTags = { ...this.metadataTags, ...tags };
    return this;
  }
}

/** sha256 hex of `bytes`. */
function sha256Hex(bytes: Uint8Array): string {
  return bytesToHex(sha256(bytes));
}

/** Truncate to at most `max` Unicode code points, appending an elision marker. */
function truncateChars(s: string, max: number): string {
  // `Array.from` / spread iterates by code point, so surrogate pairs count as
  // one — matching Rust's `chars()` (Unicode scalar values).
  const points = Array.from(s);
  if (points.length <= max) {
    return s;
  }
  return points.slice(0, max).join("") + "…";
}

/**
 * Build the privacy-reduced `[argsHash, signatureHash, argsBytes]` triple for a
 * tool call's arguments.
 *
 * - `argsHash`  = sha256 hex of the canonical (compact) JSON of `args`, or `""`
 *   when there are no args.
 * - `signatureHash` = sha256 hex of the sorted top-level arg key names joined
 *   by `,`; the literal `"none"` when args are absent or not a plain object.
 * - `argsBytes` = UTF-8 byte length of that JSON.
 */
function hashArgs(args: unknown): [string, string, number] {
  if (args === undefined || args === null) {
    return ["", "none", 0];
  }
  // Compact JSON (no spaces) mirrors Rust's `serde_json::to_string`.
  const serialized = JSON.stringify(args) ?? "";
  const serializedBytes = utf8ToBytes(serialized);
  const argsHash = sha256Hex(serializedBytes);

  let signature: string;
  if (isPlainObject(args)) {
    const keys = Object.keys(args).sort();
    signature = sha256Hex(utf8ToBytes(keys.join(",")));
  } else {
    // Arrays, numbers, strings, booleans: no top-level key names.
    signature = "none";
  }
  return [argsHash, signature, serializedBytes.length];
}

/** Whether `v` is a plain JSON object (not an array, null, or primitive). */
function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/**
 * Format a `Date` as RFC3339 UTC with millisecond precision, e.g.
 * `"2026-06-19T00:00:00.000Z"`. `Date.toISOString()` already produces exactly
 * this shape.
 */
function rfc3339(d: Date): string {
  return d.toISOString();
}

/**
 * Build the redacted excerpt from a call's prompt + completion, honoring the
 * configured redaction policy and (for the standard path) the 320-char cap.
 * Returns `undefined` when there is no text to send.
 */
export function buildExcerpt(cfg: Config, call: LlmCall): string | undefined {
  let joined = "";
  if (call.prompt !== undefined) {
    joined += call.prompt;
  }
  if (call.completion !== undefined) {
    if (joined.length > 0) {
      joined += "\n---\n";
    }
    joined += call.completion;
  }
  if (joined.length === 0) {
    return undefined;
  }

  const scrubbed = cfg.redaction === "floor" ? redact(joined).text : joined;

  // Raw mode ships the full (redacted) turns for server-side summarization; the
  // standard path caps the excerpt at 320 code points.
  return cfg.sendsFullTurns()
    ? scrubbed
    : truncateChars(scrubbed, EXCERPT_MAX_CHARS);
}

/**
 * Resolve the per-event metadata: `Config` defaults are the base layer, the
 * per-call ambient snapshot is the middle layer, and per-call tags are the top
 * layer (each later layer wins on a shared key). The caps are then applied.
 * Returns `undefined` when the merged map is empty so the wire key is omitted.
 */
export function resolveMetadata(cfg: Config, call: LlmCall): Metadata | undefined {
  const merged: Metadata = {
    ...cfg.metadata,
    ...(call.ambientMetadataSnapshot ?? {}),
    ...call.metadataTags,
  };
  if (Object.keys(merged).length === 0) {
    return undefined;
  }
  const capped = capMetadata(merged);
  return Object.keys(capped).length === 0 ? undefined : capped;
}

/**
 * Assign `key` on `target` only when `value` is defined. Keeps optional wire
 * keys *omitted entirely* when absent (never serialized as `null`).
 */
function setIfPresent<T, K extends keyof T>(
  target: T,
  key: K,
  value: T[K] | undefined,
): void {
  if (value !== undefined) {
    target[key] = value;
  }
}

/** Convert one captured call into a wire event plus its tool-call records. */
function eventFromCall(
  cfg: Config,
  call: LlmCall,
  seq: number,
): [RawEvent, ToolCallWire[]] {
  const sourceRef = `${call.sessionId}::${call.startedAt.getTime()}::${seq}`;
  const srcEventId = sourceEventId(cfg.deviceId, sourceRef);

  const contentExcerpt = buildExcerpt(cfg, call);

  // Required keys first; optionals are added only when present so they don't
  // serialize as null.
  const event: RawEvent = {
    source_event_id: srcEventId,
    ts: rfc3339(call.startedAt),
    kind: call.kind,
    agent: cfg.agent,
    provider: call.provider,
    session_id: call.sessionId,
    tokens: call.tokenUsage,
  };
  setIfPresent(event, "model", call.modelName);
  setIfPresent(event, "cwd", call.cwd);
  setIfPresent(event, "git", call.git);
  setIfPresent(event, "duration_ms", call.durationMs);
  setIfPresent(event, "content_excerpt", contentExcerpt);
  setIfPresent(event, "metadata", resolveMetadata(cfg, call));

  const toolCalls: ToolCallWire[] = call.toolCalls.map((tc, i) => {
    const [argsHash, signatureHash, argsBytes] = hashArgs(tc.args);
    // 16-hex truncation for tool-call ids (vs 32 for events/batches).
    const externalCallId = `tc_${contentHash([srcEventId, String(i)]).slice(0, 16)}`;

    const wireTc: ToolCallWire = {
      external_call_id: externalCallId,
      session_id: call.sessionId,
      source_event_id: srcEventId,
      agent: cfg.agent,
      server: tc.server ?? "builtin",
      name: tc.name,
      call_index: i,
      started_at: rfc3339(tc.startedAt ?? call.startedAt),
      status: tc.status,
      args_hash: argsHash,
      signature_hash: signatureHash,
      args_bytes: argsBytes,
      result_bytes: tc.resultBytes ?? 0,
    };
    setIfPresent(wireTc, "ended_at", tc.endedAt ? rfc3339(tc.endedAt) : undefined);
    setIfPresent(wireTc, "model", call.modelName);
    // command_families: omit entirely when empty, cap at 3.
    const families = (tc.commandFamilies ?? []).slice(0, 3);
    if (families.length > 0) {
      wireTc.command_families = families;
    }
    return wireTc;
  });

  return [event, toolCalls];
}

/**
 * A mutable monotonic sequence counter, passed by reference so per-call dedupe
 * keys stay distinct across flushes within a run (mirrors Rust's `&mut u64`).
 */
export interface SeqRef {
  value: number;
}

/**
 * Drain a batch of captured calls into a wire {@link IngestBatch}. `seqRef` is
 * a monotonic counter bumped once per call to keep per-call dedupe keys
 * distinct within a run.
 *
 * `tool_calls` is omitted entirely from the batch when no call had any (never
 * serialized as `[]`).
 */
export function buildBatch(
  cfg: Config,
  calls: Iterable<LlmCall>,
  seqRef: SeqRef,
): IngestBatch {
  const events: RawEvent[] = [];
  const toolCalls: ToolCallWire[] = [];
  const sourceIds: string[] = [];

  for (const call of calls) {
    seqRef.value += 1;
    const [event, tcs] = eventFromCall(cfg, call, seqRef.value);
    sourceIds.push(event.source_event_id);
    for (const tc of tcs) {
      toolCalls.push(tc);
    }
    events.push(event);
  }

  const batch: IngestBatch = {
    batch_id: batchId(sourceIds),
    device_id: cfg.deviceId,
    daemon_version: cfg.version,
    events,
  };
  // Omit `tool_calls` entirely when empty (additive wire contract).
  if (toolCalls.length > 0) {
    batch.tool_calls = toolCalls;
  }
  // Always send an explicit value reflecting the config so backend usage is
  // off-by-default but users can opt in. `setIfPresent` keeps the wire key
  // omitted only if the flag were ever undefined; the config default makes it a
  // concrete boolean.
  setIfPresent(batch, "auto_taxonomy", cfg.autoTaxonomy);
  return batch;
}
