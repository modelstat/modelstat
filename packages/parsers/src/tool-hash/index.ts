/**
 * Shared tool_call hashing + identity helpers — one implementation for
 * every parser (claude-code, codex) so the wire values stay byte-identical
 * across agents.
 *
 * PRIVACY: these helpers exist so raw tool arguments/results never leave
 * the device. Only one-way sha256 digests and byte lengths are derived
 * here; the inputs themselves are discarded by the caller after hashing
 * (see ToolCallWire in @modelstat/core/schemas for the wire contract).
 *
 * Node-only (node:crypto) — parsers never run in the extension.
 */
import { createHash } from "node:crypto";

/** Sentinel for signature_hash when input is not a non-empty object. */
export const SIGNATURE_NONE = "none";

export interface ArgsHashes {
  /** Hex sha256 of JSON.stringify(input); `""` when no input. */
  args_hash: string;
  /** Hex sha256 of the sorted top-level arg key names joined by `,`;
   * the literal `none` when input is not a non-empty plain object. */
  signature_hash: string;
  /** UTF-8 byte length of JSON.stringify(input); 0 when no input. */
  args_bytes: number;
}

/** Hash a tool_use `input` into the three wire fields. `undefined`/`null`
 * (and non-serialisable values) count as "no input". */
export function hashArgs(input: unknown): ArgsHashes {
  if (input === undefined || input === null) {
    return { args_hash: "", signature_hash: SIGNATURE_NONE, args_bytes: 0 };
  }
  const json = JSON.stringify(input);
  if (json === undefined) {
    // Functions/symbols — not serialisable, treat as no input.
    return { args_hash: "", signature_hash: SIGNATURE_NONE, args_bytes: 0 };
  }
  const argsHash = createHash("sha256").update(json).digest("hex");
  let signatureHash = SIGNATURE_NONE;
  if (typeof input === "object" && !Array.isArray(input)) {
    const keys = Object.keys(input as Record<string, unknown>).sort();
    if (keys.length > 0) {
      signatureHash = createHash("sha256").update(keys.join(",")).digest("hex");
    }
  }
  return {
    args_hash: argsHash,
    signature_hash: signatureHash,
    args_bytes: Buffer.byteLength(json, "utf8"),
  };
}

/** UTF-8 byte length of JSON.stringify(value) — for result_bytes.
 * `undefined`/`null`/`""` (unmatched or empty result) → 0. */
export function jsonBytes(value: unknown): number {
  if (value === undefined || value === null || value === "") return 0;
  const json = JSON.stringify(value);
  return json === undefined ? 0 : Buffer.byteLength(json, "utf8");
}

/** Full UUIDs first (their dashed groups are individually < 8 chars),
 * then any hex run of ≥8 chars that ends a name segment. */
const UUID_RE = /[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}/g;
const HEX_TAIL_RE = /[0-9a-fA-F]{8,}(?=$|[^0-9a-zA-Z])/g;

/**
 * Tool-name normalization: NFC, trim, hex/UUID-ish tails of
 * ≥8 chars inside a name segment → `<dyn>`, cap 120 chars. Keeps stable
 * vendor identifiers verbatim while collapsing per-session dynamic names
 * (`subscribe_a1b2c3d4e5f6` → `subscribe_<dyn>`).
 */
export function normalizeToolName(raw: string): string {
  const cleaned = raw
    .normalize("NFC")
    .trim()
    .replace(UUID_RE, "<dyn>")
    .replace(HEX_TAIL_RE, "<dyn>");
  return cleaned.slice(0, 120);
}

/** Split an observed tool name into wire `server` + `name`:
 * `mcp__<server>__<tool>` → `{ server: "mcp:<server>", name: "<tool>" }`,
 * anything else → `{ server: "builtin", name }`. Both parts normalized. */
export function splitObservedToolName(observed: string): { server: string; name: string } {
  const m = /^mcp__([^_].*?)__(.+)$/.exec(observed.trim());
  if (m?.[1] && m[2]) {
    return {
      server: `mcp:${normalizeToolName(m[1]).slice(0, 116)}`,
      name: normalizeToolName(m[2]),
    };
  }
  return { server: "builtin", name: normalizeToolName(observed) };
}

/** Canonical tool identity: aggregate-map key, taxonomy leaf
 * name, segment-tag name, facet value. `Bash` for builtins,
 * `mcp:<server>/<tool>` for MCP tools. */
export function toolIdentity(server: string, name: string): string {
  return server === "builtin" ? name : `${server}/${name}`;
}

/** Deterministic fallback external_call_id when the source line carries
 * no id: `tc_<djb2-base36>` of `${sourceEventId}|${callIndex}` — same
 * djb2-64 family as sourceEventId/segmentId in @modelstat/core/ids. */
export function fallbackCallId(sourceEventId: string, callIndex: number): string {
  const s = `${sourceEventId}|${callIndex}`;
  let h = 5381n;
  for (let i = 0; i < s.length; i++) {
    h = (h * 33n) ^ BigInt(s.charCodeAt(i));
    h &= 0xffffffffffffffffn;
  }
  return `tc_${h.toString(36)}`;
}
