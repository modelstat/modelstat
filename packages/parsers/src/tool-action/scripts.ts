/**
 * Detect every script/bash file a command references, and resolve each to a
 * real on-disk path — deterministic and maximally liberal, with NO
 * project-structure assumptions. Detection is pure-string (browser-safe);
 * resolution takes an injectable `exists` so it is testable and runtime-
 * agnostic (Node `fs.existsSync` in prod). A command can reference several
 * scripts; they are returned in command order so each abstract stays
 * positionally linked to its place in the (redacted) command.
 */

/** Shell metacharacters that separate command segments (`&&`, `||`, `;`, `|`, `&`, newlines). */
const SEGMENT_SEP = /\s*(?:&&|\|\||[;|&\n])\s*/;

/** A trailing script-ish file extension — a generic structural signal, not a
 * tool/category vocabulary. */
const SCRIPT_EXT =
  /\.(sh|bash|zsh|ksh|fish|py|rb|js|mjs|cjs|ts|tsx|pl|php|lua|ps1|bat|cmd|r|jl|groovy|kts)$/i;

/** Every script/bash file the command references, in order of appearance. The
 * returned tokens are exactly as they appear in `command` (pre-redaction), so
 * the caller can resolve them on disk and also map them to the redacted form. */
export function detectScriptRefs(command: string): string[] {
  const refs: string[] = [];
  for (const segment of command.split(SEGMENT_SEP)) {
    const tokens = segment.trim().split(/\s+/).filter(Boolean);
    tokens.forEach((tok, i) => {
      const t = stripQuotes(tok);
      if (!t || t.startsWith("-") || t.includes("://")) return; // flags, URLs
      const looksLikeScript =
        SCRIPT_EXT.test(t) || // ends in a script extension
        t.startsWith("./") ||
        t.startsWith("../") || // an explicit relative executable
        (i === 0 && t.includes("/")); // a leading path-to-an-executable
      if (looksLikeScript) refs.push(t);
    });
  }
  return refs;
}

/** Candidate paths to try for a referenced script, ordered MOST-ABSOLUTE /
 * LONGEST first (so the most specific existing file wins). `roots` are
 * directories from the session/event context (cwd, git root, nearby cwds, …). */
export function scriptCandidates(ref: string, roots: readonly string[]): string[] {
  const out: string[] = [];
  const push = (p: string) => {
    if (p && !out.includes(p)) out.push(p);
  };
  if (isAbsolute(ref)) push(ref);
  for (const root of roots) {
    if (root) push(joinPath(root, ref));
  }
  push(ref); // bare, relative to the process cwd
  return out.sort(
    (a, b) => Number(isAbsolute(b)) - Number(isAbsolute(a)) || b.length - a.length,
  );
}

/** Resolve a script ref to the first existing candidate (most-specific first),
 * or null when none exists. `exists` is injected — Node `fs.existsSync` in
 * prod, a fake in tests. The LLM-prediction fallback for genuinely-ambiguous
 * refs is layered on by the caller. */
export function resolveScriptPath(
  ref: string,
  roots: readonly string[],
  exists: (path: string) => boolean,
): string | null {
  for (const cand of scriptCandidates(ref, roots)) {
    if (exists(cand)) return cand;
  }
  return null;
}

function isAbsolute(p: string): boolean {
  return p.startsWith("/") || /^[A-Za-z]:[\\/]/.test(p);
}

function joinPath(root: string, rel: string): string {
  return `${root.replace(/\/+$/, "")}/${rel.replace(/^\.\//, "")}`;
}

function stripQuotes(token: string): string {
  if (token.length >= 2) {
    const a = token[0];
    const b = token[token.length - 1];
    if ((a === "'" && b === "'") || (a === '"' && b === '"')) return token.slice(1, -1);
  }
  return token;
}
