/**
 * Minimal JSONPath evaluator. Supports ONLY the subset the adapter
 * protocol needs:
 *
 *   $                 — root
 *   .name             — child by name
 *   ["name"]          — child by bracket-name (for weird keys)
 *   [N]               — array index
 *   [*]               — array spread: concatenate all children (used
 *                       for ChatGPT's `content.parts[*]`)
 *
 * No wildcards in property names, no filters, no descendant (..), no
 * unions. If you need more, add it here — the interpreter is the ONLY
 * place new syntax can land; configs can't smuggle it in.
 */

export function jsonPath(root: unknown, expr: string): unknown {
  if (expr === "$") return root;
  if (!expr.startsWith("$")) return null;
  let cur: unknown = root;
  let i = 1;
  while (i < expr.length) {
    const ch = expr[i]!;
    if (ch === ".") {
      i++;
      let name = "";
      while (i < expr.length && /[A-Za-z0-9_]/.test(expr[i]!)) {
        name += expr[i];
        i++;
      }
      if (!name) return null;
      if (cur && typeof cur === "object" && name in (cur as Record<string, unknown>)) {
        cur = (cur as Record<string, unknown>)[name];
      } else {
        return null;
      }
    } else if (ch === "[") {
      const end = expr.indexOf("]", i);
      if (end < 0) return null;
      const body = expr.slice(i + 1, end);
      i = end + 1;
      if (body === "*") {
        if (!Array.isArray(cur)) return null;
        // Spread: collect all items as a flat array
        const rest = expr.slice(i);
        // If nothing follows, return the array itself; if something
        // follows, apply recursively to each and concat.
        if (rest.length === 0) return cur;
        const collected: unknown[] = [];
        for (const item of cur) {
          const v = jsonPath(item, `$${rest}`);
          if (v === null || v === undefined) continue;
          if (Array.isArray(v)) collected.push(...v);
          else collected.push(v);
        }
        return collected;
      }
      if (body.startsWith('"') && body.endsWith('"')) {
        const key = body.slice(1, -1);
        if (cur && typeof cur === "object") {
          cur = (cur as Record<string, unknown>)[key];
        } else return null;
      } else if (/^\d+$/.test(body)) {
        const idx = Number(body);
        if (Array.isArray(cur)) cur = cur[idx];
        else return null;
      } else {
        return null;
      }
    } else {
      return null;
    }
    if (cur === undefined || cur === null) return null;
  }
  return cur ?? null;
}

/** Coerce a jsonPath result to a string (joining arrays of strings). */
export function asString(v: unknown): string | null {
  if (v === null || v === undefined) return null;
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  if (Array.isArray(v)) {
    const parts = v.map((x) => (typeof x === "string" ? x : typeof x === "object" ? JSON.stringify(x) : String(x)));
    return parts.join("");
  }
  return null;
}

/** Coerce to non-negative integer, or null. */
export function asInt(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return Math.max(0, Math.floor(v));
  if (typeof v === "string" && /^\d+$/.test(v)) return Number(v);
  return null;
}
