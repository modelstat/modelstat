/**
 * Wire-boundary byte-clamp for bounded strings.
 *
 * The wire schema (`@modelstat/core/schemas`) bounds strings with Zod
 * `.max(N)`, which counts UTF-16 CODE UNITS. The ingest server enforces the
 * SAME numbers in UTF-8 BYTES (Rust `String::len()`). Multibyte text (CJK,
 * emoji, accents — routine in real transcripts) is 2-4 bytes per code unit, so a
 * string that passes the client's `.max(N)` can be up to ~3N bytes and get a
 * PERMANENT `400 "… > N"` from the server. Since the daemon never drops a batch
 * (it holds + retries every non-2xx), such a payload wedges that file's cursor
 * forever. A concrete case: a 400-code-unit abstract of CJK is 1200 bytes and
 * the server rejects `abstract > 512`.
 *
 * Fix at the wire boundary — the same place {@link wellFormedStringify} already
 * reconciles a sibling client/server string mismatch (lone surrogates → strict
 * JSON 400): clamp every bounded string to its schema `.max` measured in BYTES.
 * This is provably safe for ANY unit the server might use, because
 *   byteLength ≥ codeUnitLength ≥ codePointCount
 * always — so ≤N bytes is also ≤N code units and ≤N code points. Driving it off
 * the schema (rather than a hand-listed set of fields) means every current AND
 * future bounded string is covered automatically; the field-by-field version is
 * exactly what let this bug ship.
 */
import type { z } from "zod";

const utf8ByteLength: (s: string) => number =
  typeof Buffer !== "undefined" && typeof Buffer.byteLength === "function"
    ? (s) => Buffer.byteLength(s, "utf8")
    : (() => {
        const enc = new TextEncoder();
        return (s: string) => enc.encode(s).length;
      })();

/**
 * Truncate `s` so its UTF-8 encoding is ≤ `maxBytes`, cutting only on a code-
 * POINT boundary (never splitting a character or a surrogate pair). Returns `s`
 * unchanged when it already fits (covers the ASCII fast path, where bytes ===
 * code units). No ellipsis: this is invisible wire plumbing and the marker would
 * corrupt structured fields (slugs, urls) it may also touch.
 */
export function clampUtf8Bytes(s: string, maxBytes: number): string {
  if (maxBytes <= 0) return "";
  if (utf8ByteLength(s) <= maxBytes) return s;
  let bytes = 0;
  let out = "";
  // `for…of` iterates whole code points, so a surrogate pair counts as one step
  // (4 bytes) and is kept or dropped atomically — the cut is never mid-character.
  for (const ch of s) {
    const b = utf8ByteLength(ch);
    if (bytes + b > maxBytes) break;
    bytes += b;
    out += ch;
  }
  return out;
}

/** The bounded-string cap on a ZodString, or null if it declares no `.max`. */
function stringMax(schema: z.ZodString): number | null {
  for (const check of schema._def.checks) {
    if (check.kind === "max") return check.value;
  }
  return null;
}

/**
 * Recursively clamp every bounded string in `value` to its schema's `.max`,
 * measured in UTF-8 BYTES (see {@link clampUtf8Bytes}). Unknown/unbounded nodes
 * pass through untouched, and unchanged subtrees keep their original reference
 * (structural sharing — no needless cloning of a batch that's already ASCII).
 *
 * Dispatch is on `_def.typeName` rather than `instanceof` so it stays correct
 * even if more than one copy of Zod is resolved in the dependency graph.
 */
export function clampToSchemaBytes<T>(schema: z.ZodTypeAny, value: T): T {
  if (value === null || value === undefined) return value;
  const def = schema._def as { typeName?: string; [k: string]: unknown };
  switch (def.typeName) {
    // Unwrap the modifiers that don't change the on-the-wire shape.
    case "ZodOptional":
    case "ZodNullable":
    case "ZodDefault":
    case "ZodCatch":
      return clampToSchemaBytes(def.innerType as z.ZodTypeAny, value);
    case "ZodEffects":
      return clampToSchemaBytes(def.schema as z.ZodTypeAny, value);
    case "ZodBranded":
    case "ZodReadonly":
      return clampToSchemaBytes(def.type as z.ZodTypeAny, value);
    case "ZodString": {
      const max = stringMax(schema as z.ZodString);
      return max != null && typeof value === "string" ? (clampUtf8Bytes(value, max) as T) : value;
    }
    case "ZodArray": {
      if (!Array.isArray(value)) return value;
      const el = def.type as z.ZodTypeAny;
      let changed = false;
      const out = value.map((v) => {
        const c = clampToSchemaBytes(el, v);
        if (c !== v) changed = true;
        return c;
      });
      return (changed ? out : value) as T;
    }
    case "ZodObject": {
      const shape = (def.shape as () => Record<string, z.ZodTypeAny>)();
      let changed = false;
      const out: Record<string, unknown> = {};
      for (const key of Object.keys(value as object)) {
        const child = shape[key];
        const v = (value as Record<string, unknown>)[key];
        const c = child ? clampToSchemaBytes(child, v) : v;
        if (c !== v) changed = true;
        out[key] = c;
      }
      return (changed ? out : value) as T;
    }
    case "ZodRecord": {
      const valueType = def.valueType as z.ZodTypeAny;
      let changed = false;
      const out: Record<string, unknown> = {};
      for (const key of Object.keys(value as object)) {
        const v = (value as Record<string, unknown>)[key];
        const c = clampToSchemaBytes(valueType, v);
        if (c !== v) changed = true;
        out[key] = c;
      }
      return (changed ? out : value) as T;
    }
    default:
      return value;
  }
}
