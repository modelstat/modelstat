/**
 * ID helpers. modelstat IDs are ULIDs almost everywhere (sortable, URL-safe,
 * 26 chars) — except for provider-side identifiers which we keep in their
 * native format to make reconciliation easy.
 *
 * All id helpers are runtime-agnostic: they work in Node 20+ and in the
 * browser (Chrome extension service worker). See
 * docs/daemon-unification.md for the policy.
 */
/**
 * Inline ULID — runtime-agnostic (Node 20+, MV3 service worker, browser).
 *
 * We don't use the `ulid` npm package because newer versions check for
 * `window.crypto` at import time and throw "secure crypto unusable" in MV3
 * service workers (no `window`). globalThis.crypto.getRandomValues exists
 * in all target runtimes.
 */
const ULID_ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

function ulid(seedTime?: number): string {
  const t = seedTime ?? Date.now();
  let timeStr = "";
  let ts = t;
  for (let i = 9; i >= 0; i--) {
    timeStr = ULID_ALPHABET[ts % 32] + timeStr;
    ts = Math.floor(ts / 32);
  }
  const bytes = new Uint8Array(16);
  globalThis.crypto.getRandomValues(bytes);
  let randStr = "";
  for (let i = 0; i < 16; i++) {
    randStr += ULID_ALPHABET[bytes[i]! % 32];
  }
  return timeStr + randStr;
}

export const newId = (): string => ulid();

/** Alias used by the ingest pipeline when creating a new IngestBatch. */
export const batchId = (): string => ulid();

/** UUIDv7 — time-ordered, v7-tagged. Uses the platform's crypto.randomUUID()
 * to emit a v4 string then rewrites the timestamp + version nibble. Works
 * in both Node 19+ and modern browsers. */
export function uuidv7(): string {
  // Start from a v4 from the platform RNG (both Node and browser have it).
  const v4 = crypto.randomUUID();
  const ms = Date.now();
  const msHex = ms.toString(16).padStart(12, "0");
  // v4 shape: xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx
  // v7 shape: hhhhhhhh-hhhh-7xxx-yxxx-xxxxxxxxxxxx (h = big-endian ms)
  const part0 = msHex.slice(0, 8);
  const part1 = msHex.slice(8, 12);
  const part2 = `7${v4.slice(15, 18)}`; // keep last 3 random hex digits, flip version
  const rest = v4.slice(19); // "yxxx-xxxxxxxxxxxx"
  return `${part0}-${part1}-${part2}-${rest}`;
}

/**
 * Deterministic dedupe key for a single parsed event.
 *
 * Three legal shapes for `source`:
 *   - `{ file, byteOffset }` — CLI daemon parsing JSONL
 *   - `{ host, conversationId, messageId }` — Chrome extension capturing
 *     a web chat turn
 *   - `{ lineUuid }` — position-independent key for transcript lines whose
 *     identity is the uuid embedded in the line itself. Used for lines that
 *     tools copy verbatim into new files (Claude Code resume transcripts):
 *     a (file, byteOffset) key would mint a fresh id per copy and the
 *     server would count the same turn twice.
 *
 * Output format: `evt_<base36(djb2-64)>`. djb2 is not cryptographic but is
 * collision-free at our event scale (< 1e10 events lifetime) and keeps ids
 * short. Deterministic across Node + browser; no Web Crypto needed.
 *
 * Legacy signature `sourceEventId(deviceId, filePath, byteOffset)` is
 * preserved for backward compat — existing callers don't change. The key
 * derivation for existing shapes is a wire contract: the server dedupes on
 * (scope_id, source_event_id), so changing how a shape hashes would re-ingest
 * all previously-uploaded history under new ids.
 */
export function sourceEventId(
  deviceId: string,
  sourceOrFilePath:
    | string
    | { file: string; byteOffset: number }
    | { host: string; conversationId: string; messageId: string }
    | { lineUuid: string },
  byteOffsetMaybe?: number,
): string {
  let key: string;
  if (typeof sourceOrFilePath === "string") {
    // Legacy 3-arg form.
    const byteOffset = byteOffsetMaybe ?? 0;
    key = `fs::${sourceOrFilePath}::${byteOffset}`;
  } else if ("file" in sourceOrFilePath) {
    key = `fs::${sourceOrFilePath.file}::${sourceOrFilePath.byteOffset}`;
  } else if ("lineUuid" in sourceOrFilePath) {
    key = `uuid::${sourceOrFilePath.lineUuid}`;
  } else {
    key = `web::${sourceOrFilePath.host}::${sourceOrFilePath.conversationId}::${sourceOrFilePath.messageId}`;
  }
  const s = `${deviceId}::${key}`;
  // djb2 hash, base36
  let h = 5381n;
  for (let i = 0; i < s.length; i++) {
    h = (h * 33n) ^ BigInt(s.charCodeAt(i));
    h &= 0xffffffffffffffffn;
  }
  return `evt_${h.toString(36)}`;
}

/**
 * Deterministic id for a segment. Re-running the summariser pipeline on the
 * exact same input events produces the same id → upserts are idempotent at
 * the segment level (see docs/ingestion-pipeline.md "Idempotent at every
 * boundary").
 *
 * Uses djb2-64 like sourceEventId so it runs without Web Crypto (service
 * workers inherit globalThis.crypto but we prefer synchronous code here).
 * Collision resistance is sufficient: ~1e5 segments per user lifetime
 * keeps birthday probability negligible.
 */
export function segmentId(
  sessionId: string,
  startedAtMs: number,
  endedAtMs: number,
  sourceEventIds: readonly string[],
): string {
  const sorted = [...sourceEventIds].sort().join(",");
  const s = `${sessionId}|${startedAtMs}|${endedAtMs}|${sorted}`;
  let h = 5381n;
  for (let i = 0; i < s.length; i++) {
    h = (h * 33n) ^ BigInt(s.charCodeAt(i));
    h &= 0xffffffffffffffffn;
  }
  return `seg_${h.toString(36)}`;
}

/**
 * Deterministic fallback `external_call_id` for a tool call whose source line
 * carries no id of its own: `tc_<djb2-base36>` of `${eventId}|${callIndex}`.
 *
 * Same djb2-64 family as sourceEventId/segmentId above — a tool call is keyed
 * by the event that carried it plus its ordinal within that event, so the id is
 * stable across re-parses and the server's upserts stay idempotent.
 *
 * CROSS-REPO CONTRACT: `modelstat_wire::ids::tc_fallback_id` reproduces this
 * bit-for-bit; the golden vectors in `daemon/crates/modelstat-wire/tests/golden/
 * ids.json` (`tc_fallback_id`) pin it.
 */
export function fallbackCallId(eventId: string, callIndex: number): string {
  const s = `${eventId}|${callIndex}`;
  let h = 5381n;
  for (let i = 0; i < s.length; i++) {
    h = (h * 33n) ^ BigInt(s.charCodeAt(i));
    h &= 0xffffffffffffffffn;
  }
  return `tc_${h.toString(36)}`;
}

/**
 * Deterministic, value-masked "shape" of a command's arguments — the tier-1
 * skeleton that may ride the wire. Generic and privacy-first: it keeps
 * STRUCTURE (flags) and masks every value to `§`, so no raw value can leak.
 * The executable is NOT part of `args` — the caller passes the command with
 * the leading program already stripped (it ships separately as `executable`).
 *
 * CROSS-REPO CONTRACT: the backend's Rust implementation reproduces this
 * bit-for-bit — pinned by the golden vectors in ids.test.ts and a matching
 * backend test. Rules: tokenize on ASCII whitespace; a `-`-leading token is a
 * flag, kept verbatim except `--key=value` → `--key=§`; every other
 * (positional/value) token → `§`; join with single spaces.
 *
 * NOTE: conservative for now — masks ALL positionals, including leading
 * subcommands. Whether to keep subcommand keywords is an open policy question
 * (utility vs. leak risk).
 */
export function paramShape(args: string): string {
  const MASK = "§";
  return args
    .split(/[ \t\n\r\f]+/)
    .filter((t) => t.length > 0)
    .map((t) => {
      if (t.startsWith("-")) {
        const eq = t.indexOf("=");
        return eq === -1 ? t : `${t.slice(0, eq + 1)}${MASK}`;
      }
      return MASK;
    })
    .join(" ");
}
