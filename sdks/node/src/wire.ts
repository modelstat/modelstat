/**
 * The ingest wire contract, as a **self-contained** set of types plus the
 * deterministic id derivation.
 *
 * This package is Apache-2.0 and must not depend on the (BSL-licensed) server
 * `modelstat-core`, so the shapes that cross `POST /v1/ingest` are re-declared
 * here. They mirror `modelstat-core`'s `RawEvent` / `ToolCallWire` /
 * `IngestBatch` field-for-field; the golden-vector tests pin the deterministic
 * id derivation to the server's algorithm so the two can never silently drift.
 * Ids ride the wire as plain strings (the server deserializes them into its
 * typed newtypes).
 *
 * The JSON keys are deliberately snake_case to match the server byte-for-byte:
 * a missing or misnamed REQUIRED field is an HTTP 400 that rejects the whole
 * batch. Optional keys must be **omitted entirely** when absent (never sent as
 * `null`).
 *
 * PRIVACY INVARIANT (mirrors the server contract): tool-call records carry only
 * hashes, byte sizes, and allowlisted command verbs — never raw args, results,
 * paths, or command text.
 */

import { blake3 } from "@noble/hashes/blake3.js";
import { bytesToHex, utf8ToBytes } from "@noble/hashes/utils.js";

/**
 * The five token classes (a fixed taxonomy). Counts default to zero. The whole
 * object is always serialized (even all-zero) so the server sees a stable
 * shape.
 */
export interface TokenUsage {
  input: number;
  output: number;
  cache_creation: number;
  cache_read: number;
  reasoning: number;
}

/** A `TokenUsage` with every class zeroed. */
export function zeroTokens(): TokenUsage {
  return {
    input: 0,
    output: 0,
    cache_creation: 0,
    cache_read: 0,
    reasoning: 0,
  };
}

/** Saturating-style sum across all five token classes. */
export function totalTokens(t: TokenUsage): number {
  return (
    t.input + t.output + t.cache_creation + t.cache_read + t.reasoning
  );
}

/** The structural kind of a source event (snake_case on the wire). */
export type EventKind =
  | "user_message"
  | "assistant_message"
  | "tool_call"
  | "tool_result"
  | "summary";

/**
 * How the provider billed the call (snake_case on the wire).
 *
 * Stated by the caller, never guessed downstream — only the caller knows
 * whether the credential behind the call was a flat plan or a metered key.
 * `"unknown"` is a legal answer: it prices to $0 and is reported as
 * unattributed usage, rather than being silently billed at list price.
 */
export type PricingMode = "subscription" | "api" | "unknown";

/** Outcome of a tool invocation (snake_case on the wire). */
export type ToolCallStatus =
  | "success"
  | "error"
  | "denied"
  | "timeout"
  | "unknown";

/** Git context captured at the moment of the call (all optional). */
export interface GitContext {
  remote_slug?: string;
  host?: string;
  branch?: string;
}

/**
 * One LLM call as it crosses the ingest boundary: small and numeric, with at
 * most a short redacted excerpt of text.
 *
 * Optional fields are typed as `?:` and MUST be omitted from the JSON when
 * absent — the builder in `capture.ts` only assigns keys that are present.
 */
export interface RawEvent {
  source_event_id: string;
  /** RFC3339 UTC, e.g. `"2026-06-19T00:00:00.000Z"`. */
  ts: string;
  kind: EventKind;
  /**
   * The **agent** — which AI tool/integration produced the call (e.g.
   * `raw_sdk_openai`), not the provider. (The wire key is `agent`.)
   */
  agent: string;
  provider: string;
  model?: string;
  session_id: string;
  tokens: TokenUsage;
  cwd?: string;
  git?: GitContext;
  duration_ms?: number;
  /** REQUIRED — always sent. The server has no default for it. */
  pricing_mode: PricingMode;
  /**
   * Redacted excerpt used to build summaries downstream. Capped at 320 chars
   * in the standard (floor-redacted) path; carries the full redacted turns in
   * remote-raw mode, where the server summarizes.
   */
  content_excerpt?: string;
  /**
   * Free-form attribution tags (`feature`, `customer_id`, `team`, …), merged
   * from `Config` defaults, the ambient context layer, and per-call values.
   * Capped before send (see {@link capMetadata}); omitted entirely when empty.
   */
  metadata?: Metadata;
}

/** A flat `string → string` attribution tag map. */
export type Metadata = Record<string, string>;

/**
 * Client-side caps for the per-call `metadata` map, enforced before a batch
 * leaves the process so an over-large map can never trip an HTTP 400 (or bloat
 * the wire). At most {@link METADATA_MAX_ENTRIES} entries survive (excess keys
 * dropped deterministically in sorted-key order); each key is truncated to
 * {@link METADATA_MAX_KEY_CHARS} and each value to {@link METADATA_MAX_VALUE_CHARS}
 * Unicode code points.
 */
export const METADATA_MAX_ENTRIES = 16;
/** Max length of a metadata key, in Unicode code points. */
export const METADATA_MAX_KEY_CHARS = 64;
/** Max length of a metadata value, in Unicode code points. */
export const METADATA_MAX_VALUE_CHARS = 256;

/** Truncate `s` to at most `max` Unicode code points (no elision marker). */
function truncateScalars(s: string, max: number): string {
  const points = Array.from(s);
  if (points.length <= max) {
    return s;
  }
  return points.slice(0, max).join("");
}

/**
 * Apply the metadata caps to a resolved map: keep at most
 * {@link METADATA_MAX_ENTRIES} entries (the lexicographically-smallest keys, so
 * the drop is deterministic), truncating each key and value. Returns a fresh
 * object whose keys are in sorted order.
 */
export function capMetadata(map: Metadata): Metadata {
  const out: Metadata = {};
  const keys = Object.keys(map).sort();
  for (const key of keys.slice(0, METADATA_MAX_ENTRIES)) {
    const cappedKey = truncateScalars(key, METADATA_MAX_KEY_CHARS);
    out[cappedKey] = truncateScalars(map[key]!, METADATA_MAX_VALUE_CHARS);
  }
  return out;
}

/** One tool invocation, privacy-reduced. Hashes and sizes only. */
export interface ToolCallWire {
  external_call_id: string;
  session_id: string;
  source_event_id: string;
  segment_id?: string;
  /** The **agent** (AI tool) that ran the call — same space as `RawEvent.agent`. */
  agent: string;
  /** `builtin` or `mcp:<server>`. */
  server: string;
  /** Bare tool name (`Bash`, `create_pr`). */
  name: string;
  turn_index?: number;
  call_index: number;
  /** RFC3339 UTC. */
  started_at: string;
  /** RFC3339 UTC. */
  ended_at?: string;
  status: ToolCallStatus;
  /** Hex sha256 of the serialized input; `""` when the call had no input. */
  args_hash: string;
  /**
   * Sha256 of the sorted top-level arg key names joined by `,`; the literal
   * `none` when the input is not a plain object.
   */
  signature_hash: string;
  args_bytes: number;
  result_bytes: number;
  model?: string;
  /** Allowlisted command verbs (omitted entirely when empty, capped at 3). */
  command_families?: string[];
}

/**
 * The full ingest payload. The SDK only ever emits `events` (+ `tool_calls`);
 * segmentation, summarization, titles, and session-installs are produced
 * downstream by the daemon or server.
 */
export interface IngestBatch {
  batch_id: string;
  device_id: string;
  /**
   * This SDK build's version string (≤40 chars). Ships as the wire
   * `daemon_version` field — the server's name for the producing client's
   * version; an SDK is just another producer of the ingest contract.
   *
   * NOTE: the key is `daemon_version`, NOT `client_version`.
   */
  daemon_version: string;
  events: RawEvent[];
  /** Omitted entirely when empty (never send `tool_calls: []`). */
  tool_calls?: ToolCallWire[];
  /**
   * Per-batch taxonomy auto-detection toggle. Omitted/`null` = server default
   * (taxonomy auto/on); `false` = skip taxonomy auto-detection for this batch;
   * `true` = force it on. SDK/backend integrations default this to `false`
   * (backend LLM usage isn't interactive work-sessions). The builder only sets
   * it when defined.
   */
  auto_taxonomy?: boolean;
}

// ---- deterministic ids (mirror modelstat-core::ids) -------------------------

/**
 * blake3 content hash of `parts` joined by the ASCII unit separator (`0x1F`),
 * lowercase hex truncated to 32 chars. Identical to the server's `content_hash`
 * so client- and server-derived ids agree.
 *
 * The separator is inserted **between** consecutive parts only — never before
 * the first and never after the last.
 */
export function contentHash(parts: string[]): string {
  // Build the unit-separator-joined byte sequence, then hash it in one shot.
  // Equivalent to streaming `update(part)` / `update([0x1f])` like the Rust
  // reference, but a single concat keeps it dependency-light and obvious.
  const chunks: Uint8Array[] = [];
  for (let i = 0; i < parts.length; i++) {
    if (i > 0) {
      chunks.push(SEP);
    }
    chunks.push(utf8ToBytes(parts[i]!));
  }
  const joined = concatBytes(chunks);
  const full = bytesToHex(blake3(joined));
  return full.slice(0, 32);
}

/** The ASCII unit separator (0x1F) used to join `contentHash` parts. */
const SEP = new Uint8Array([0x1f]);

/** Concatenate a list of byte arrays into one. */
function concatBytes(chunks: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const c of chunks) {
    total += c.length;
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const c of chunks) {
    out.set(c, offset);
    offset += c.length;
  }
  return out;
}

/**
 * Stable per-source-event dedupe key: `evt_<contentHash(device, sourceRef)>`.
 * `sourceRef` must be stable for the same logical call across retries.
 */
export function sourceEventId(deviceId: string, sourceRef: string): string {
  return `evt_${contentHash([deviceId, sourceRef])}`;
}

/**
 * Deterministic batch id over the (sorted) source-event ids it carries, so a
 * resend of the same events reuses the id and the server's manifest dedupes it.
 */
export function batchId(sourceEventIds: string[]): string {
  // Sort ascending as strings (matches the server's `sort_unstable`).
  const ids = [...sourceEventIds].sort();
  return `batch_${contentHash(ids)}`;
}
