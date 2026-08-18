/**
 * Redaction helpers — the wire privacy floor. The daemon runs this BEFORE any
 * content hits the wire. The server may apply its own defence-in-depth checks
 * independently.
 *
 * Goal: high recall on secrets + PII; minimal false positives on code.
 *
 * The secret catalogue lives in `./redact-floor` (the single source of truth,
 * shared with the SDK redactor so the two can't drift). This module is the
 * reference implementation the golden redaction fixtures are generated from
 * (`daemon/scripts/fixtures/gen-redaction.mts`); the shipping daemon runs the
 * Rust port in `daemon/crates/modelstat-redact`, which also owns the additive
 * server-delivered `policies` augment.
 */

import { SECRET_FLOOR } from "./redact-floor.js";

export interface RedactionResult {
  text: string;
  counts: {
    secrets_found: number;
    emails_redacted: number;
    paths_redacted_absolute: number;
  };
}

const EMAIL_PATTERN = /\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi;
const ABSOLUTE_PATH_MACOS = /\/Users\/[^\s"'`)]+/g;
const ABSOLUTE_PATH_LINUX = /\/home\/[^\s"'`)]+/g;

/** Entropy-based catcher for generic high-entropy tokens (API keys we don't
 * have explicit patterns for) plus large random blobs — digests / git SHAs /
 * other hashes and base64 payloads. These carry no analytic value, can leak
 * secrets, and bloat the wire, so they are collapsed to a marker. Operates on
 * unbroken word tokens of ≥32 chars; see {@link redact} for the rules. */
function entropy(s: string): number {
  const freq = new Map<string, number>();
  for (const c of s) freq.set(c, (freq.get(c) ?? 0) + 1);
  let h = 0;
  for (const n of freq.values()) {
    const p = n / s.length;
    h -= p * Math.log2(p);
  }
  return h;
}

const TOKEN_CANDIDATE = /[A-Za-z0-9/+=_-]{32,}/g;

export function redact(text: string, repoRootAbs?: string): RedactionResult {
  let out = text;
  const counts = {
    secrets_found: 0,
    emails_redacted: 0,
    paths_redacted_absolute: 0,
  };

  // Baseline floor — always applied first, never server-weakenable.
  for (const { name, pattern } of SECRET_FLOOR) {
    out = out.replace(pattern, () => {
      counts.secrets_found += 1;
      return `[REDACTED:${name}]`;
    });
  }

  // Entropy pass — after named patterns, so it won't double-count.
  out = out.replace(TOKEN_CANDIDATE, (match) => {
    // Long pure-hex = a digest / git SHA / content hash — collapse it (privacy
    // + payload size). Hex shorter than 32 chars never reaches here.
    if (/^[a-fA-F0-9]{32,}$/.test(match)) {
      counts.secrets_found += 1;
      return "[REDACTED:hash]";
    }
    // SCREAMING_SNAKE / all-caps-or-digit constant names — not secrets.
    if (/^[A-Z0-9_]+$/.test(match)) return match;
    // Base64 / base64url blobs: trailing `=` padding or an embedded `+` are
    // strong binary-payload signals that code and paths almost never carry.
    // (`/` alone is a path separator, so it is deliberately NOT a signal —
    // redacting paths would break command readability + script-token zipping.)
    if (/=$|\+/.test(match) && entropy(match) >= 3.5) {
      counts.secrets_found += 1;
      return "[REDACTED:base64]";
    }
    // Generic high-entropy token — an API key we have no explicit pattern for.
    const hasDigit = /\d/.test(match);
    const hasUpper = /[A-Z]/.test(match);
    const hasLower = /[a-z]/.test(match);
    if (!(hasDigit && hasUpper && hasLower)) return match;
    if (entropy(match) < 3.6) return match;
    counts.secrets_found += 1;
    return "[REDACTED:hi-entropy]";
  });

  out = out.replace(EMAIL_PATTERN, () => {
    counts.emails_redacted += 1;
    return "[REDACTED:email]";
  });

  // Absolute paths: keep them IFF they are under the session's repo root
  // (in which case they're safe to map to a relative path). Otherwise redact.
  const pathReplacer = (match: string): string => {
    if (repoRootAbs && match.startsWith(repoRootAbs)) {
      return match.slice(repoRootAbs.length).replace(/^\/+/, "./");
    }
    counts.paths_redacted_absolute += 1;
    return "[REDACTED:abs-path]";
  };
  out = out.replace(ABSOLUTE_PATH_MACOS, pathReplacer);
  out = out.replace(ABSOLUTE_PATH_LINUX, pathReplacer);

  return { text: out, counts };
}
