/**
 * Compaction: shrink session payloads BEFORE upload by truncating large
 * blobs, dropping known-binary fields, and collapsing repeated identical
 * tool calls into a single entry with a count.
 *
 * Compaction is lossy. Every compacted session carries a metadata flag
 * so the dashboard can show "compacted" provenance.
 */

export type CompactOptions = {
  /** Truncate any string field longer than this many chars. */
  maxStringLength: number;
  /** Truncate `stdout` / `stderr` / `output` / `tool_output` to this many chars. */
  maxToolOutput: number;
  /** Drop known-binary fields entirely (base64 blobs, image data). */
  dropBinaryBlobs: boolean;
  /** Collapse runs of identical tool calls into one entry with `repeat: N`. */
  collapseRepeats: boolean;
};

export const DEFAULT_COMPACT: CompactOptions = {
  maxStringLength: 8 * 1024,
  maxToolOutput: 4 * 1024,
  dropBinaryBlobs: true,
  collapseRepeats: true,
};

export type CompactResult<T> = {
  data: T;
  /** Bytes shaved off the JSON-serialised input. */
  bytesSaved: number;
  /** Number of distinct truncations / drops / collapses applied. */
  changesApplied: number;
};

const TOOL_OUTPUT_FIELDS = new Set([
  "stdout",
  "stderr",
  "output",
  "tool_output",
  "raw_text",
  "response_text",
]);

const BINARY_FIELDS = new Set([
  "image_data",
  "image_b64",
  "binary",
  "blob",
  "audio_data",
]);

function looksLikeBase64Blob(s: string): boolean {
  if (s.length < 1024) return false;
  // Heuristic: predominantly base64 chars + length divisible by 4.
  const cleaned = s.replace(/\s/g, "");
  if (cleaned.length % 4 !== 0) return false;
  if (!/^[A-Za-z0-9+/=_-]+$/.test(cleaned.slice(0, 256))) return false;
  return true;
}

function truncate(s: string, max: number, label: string): { value: string; saved: number; changed: boolean } {
  if (s.length <= max) return { value: s, saved: 0, changed: false };
  const head = s.slice(0, max);
  const truncated = `${head}\n…[truncated by modelstat-daemon: ${label}, ${s.length - max} chars dropped]`;
  return { value: truncated, saved: s.length - truncated.length, changed: true };
}

function fingerprint(v: unknown): string {
  // Cheap stable signature for run-collapse — JSON of the salient
  // fields without timestamps. We only use this to detect identical
  // repeated tool calls, not for security.
  if (v === null || typeof v !== "object") return JSON.stringify(v);
  const o = v as Record<string, unknown>;
  const skip = new Set(["ts", "timestamp", "started_at", "ended_at", "duration_ms", "id"]);
  const entries = Object.entries(o)
    .filter(([k]) => !skip.has(k))
    .sort(([a], [b]) => a.localeCompare(b));
  return JSON.stringify(entries);
}

function collapseRunsInArray(arr: unknown[]): { out: unknown[]; collapsed: number } {
  if (arr.length < 2) return { out: arr, collapsed: 0 };
  const out: unknown[] = [];
  let collapsed = 0;
  let prev: { fp: string; v: unknown; count: number } | null = null;

  const flush = () => {
    if (!prev) return;
    if (prev.count === 1) {
      out.push(prev.v);
    } else {
      // Wrap in {repeat, value} only if the value is an object we can
      // augment; otherwise just push N times (preserves shape).
      if (prev.v && typeof prev.v === "object" && !Array.isArray(prev.v)) {
        out.push({ ...(prev.v as Record<string, unknown>), repeat: prev.count });
        collapsed += prev.count - 1;
      } else {
        for (let i = 0; i < prev.count; i++) out.push(prev.v);
      }
    }
  };

  for (const v of arr) {
    const fp = fingerprint(v);
    if (prev && prev.fp === fp) {
      prev.count++;
    } else {
      flush();
      prev = { fp, v, count: 1 };
    }
  }
  flush();
  return { out, collapsed };
}

/**
 * Compact a session payload. Returns a deep clone — the input is never
 * mutated. Tracks how many bytes were saved and how many distinct
 * changes were made (for provenance metadata).
 */
export function compact<T>(input: T, opts: Partial<CompactOptions> = {}): CompactResult<T> {
  const o = { ...DEFAULT_COMPACT, ...opts };
  const before = JSON.stringify(input).length;
  let changes = 0;

  const visit = (key: string | null, v: unknown): unknown => {
    if (typeof v === "string") {
      // Drop binary-looking blobs entirely.
      if (o.dropBinaryBlobs && (key !== null && BINARY_FIELDS.has(key))) {
        changes++;
        return `<dropped:binary_field:${key}:${v.length}b>`;
      }
      if (o.dropBinaryBlobs && looksLikeBase64Blob(v)) {
        changes++;
        return `<dropped:binary_blob:${v.length}b>`;
      }
      // Tool output fields get the smaller cap.
      if (key !== null && TOOL_OUTPUT_FIELDS.has(key)) {
        const t = truncate(v, o.maxToolOutput, `tool_output:${key}`);
        if (t.changed) changes++;
        return t.value;
      }
      // Generic large strings.
      const t = truncate(v, o.maxStringLength, "string");
      if (t.changed) changes++;
      return t.value;
    }
    if (Array.isArray(v)) {
      const mapped = v.map((item) => visit(key, item));
      if (o.collapseRepeats && mapped.length > 1) {
        const collapsed = collapseRunsInArray(mapped);
        if (collapsed.collapsed > 0) changes++;
        return collapsed.out;
      }
      return mapped;
    }
    if (v && typeof v === "object") {
      const out: Record<string, unknown> = {};
      for (const [k, val] of Object.entries(v)) {
        out[k] = visit(k, val);
      }
      return out;
    }
    return v;
  };

  const data = visit(null, input) as T;
  const after = JSON.stringify(data).length;
  return { data, bytesSaved: before - after, changesApplied: changes };
}
