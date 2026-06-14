import type { RawEvent, ToolCallWire } from "@modelstat/core";

/** One extracted tool invocation, before segment attribution.
 *
 * Identical to the ToolCallWire shape (and its privacy contract:
 * hashes / byte sizes / allowlisted verbs only, never payloads) minus
 * `segment_id`, which the companion fills at batch-build time once
 * segments exist — parse time is too early to know it. */
export type ToolCallDraft = Omit<ToolCallWire, "segment_id">;

/** What a parser produces for a single source file (or SQLite row-set).
 *
 * Parsers emit RawEvents only — session-level summarisation is done by
 * the companion pipeline (@modelstat/companion-core/pipeline), not here.
 * Keeps parsers cheap + deterministic. */
export interface ParseResult {
  /** All parsed events — EMPTY when the caller provided
   * `ParserContext.onEvents` (streaming mode), so a multi-hundred-MB
   * transcript never materialises as one giant array. */
  events: RawEvent[];
  /** Per-call tool invocations extracted from the source. Empty for
   * sources without tool-call data (Cursor). */
  toolCalls: ToolCallDraft[];
  stats: {
    rawLines: number;
    emittedEvents: number;
    skipped: number;
  };
  /** Source file path (for dedupe + replay). */
  sourceFile: string;
}

/** Upper bound on how many events a streaming parser hands to
 * `onEvents` per call. Bounds the parser's working set: with a sink
 * attached, at most this many events exist inside the parser at any
 * moment regardless of file size. */
export const PARSER_EVENT_CHUNK = 256;

export interface ParserContext {
  /** Stable device id. */
  deviceId: string;
  /** Absolute path to the file being parsed (or a synthetic key for DBs). */
  sourceFile: string;
  /** For incremental parsers: skip bytes before this offset. */
  byteOffsetStart?: number;
  /** Streaming sink. When set, the parser delivers events in chunks of
   * at most PARSER_EVENT_CHUNK as it reads the file and does NOT
   * accumulate them in `ParseResult.events`. The parser awaits the sink
   * before reading on, so a slow consumer (e.g. a batch flush that runs
   * the summariser) applies natural backpressure to the file read.
   *
   * This is the memory contract for full-corpus reprocesses (cursor
   * wipes): the scan loop must see a bounded number of events at a
   * time, never a whole file's worth — let alone the whole corpus. */
  onEvents?: (events: RawEvent[]) => void | Promise<void>;
}
