/**
 * Redaction helpers. The agent runs this BEFORE any content hits the wire.
 * The server may apply its own defence-in-depth checks independently.
 *
 * Goal: high recall on secrets + PII; minimal false positives on code.
 */

export interface RedactionResult {
  text: string;
  counts: {
    secrets_found: number;
    emails_redacted: number;
    paths_redacted_absolute: number;
  };
}

const SECRET_PATTERNS: ReadonlyArray<{ name: string; pattern: RegExp }> = [
  { name: "anthropic_key", pattern: /sk-ant-[A-Za-z0-9_-]{20,}/g },
  { name: "openai_key", pattern: /sk-(?:proj-)?[A-Za-z0-9_-]{20,}/g },
  { name: "google_api_key", pattern: /AIza[0-9A-Za-z_-]{35}/g },
  { name: "aws_access_key", pattern: /AKIA[0-9A-Z]{16}/g },
  { name: "aws_secret_key", pattern: /(?<![A-Za-z0-9/+=])[A-Za-z0-9/+=]{40}(?![A-Za-z0-9/+=])/g },
  { name: "github_pat", pattern: /ghp_[A-Za-z0-9]{36}/g },
  { name: "github_oauth", pattern: /gho_[A-Za-z0-9]{36}/g },
  { name: "github_app", pattern: /gh[sur]_[A-Za-z0-9]{36}/g },
  { name: "slack_token", pattern: /xox[baprs]-[A-Za-z0-9-]{10,}/g },
  { name: "stripe_live_key", pattern: /sk_live_[A-Za-z0-9]{24,}/g },
  { name: "stripe_test_key", pattern: /sk_test_[A-Za-z0-9]{24,}/g },
  { name: "jwt", pattern: /eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}/g },
  { name: "private_key_header", pattern: /-----BEGIN [A-Z ]+PRIVATE KEY-----/g },
];

const EMAIL_PATTERN = /\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi;
const ABSOLUTE_PATH_MACOS = /\/Users\/[^\s"'`)]+/g;
const ABSOLUTE_PATH_LINUX = /\/home\/[^\s"'`)]+/g;

/** Entropy-based catcher for generic high-entropy tokens (API keys we don't
 * have explicit patterns for). Flags sequences of ≥32 chars mixing case +
 * digits with Shannon entropy ≥ 3.6 bits/char. Conservative: applies only to
 * unbroken word tokens. */
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

  for (const { name, pattern } of SECRET_PATTERNS) {
    out = out.replace(pattern, () => {
      counts.secrets_found += 1;
      return `[REDACTED:${name}]`;
    });
  }

  // Entropy pass — after named patterns, so it won't double-count.
  out = out.replace(TOKEN_CANDIDATE, (match) => {
    // skip obvious non-secrets: pure hex looking like a git SHA, long words
    if (/^[a-f0-9]+$/i.test(match)) return match;
    if (/^[A-Z]+$/.test(match)) return match; // all caps constants
    const hasLetter = /[A-Za-z]/.test(match);
    const hasDigit = /\d/.test(match);
    const hasUpper = /[A-Z]/.test(match);
    const hasLower = /[a-z]/.test(match);
    if (!(hasLetter && hasDigit && hasUpper && hasLower)) return match;
    if (entropy(match) < 3.6) return match;
    counts.secrets_found += 1;
    return `[REDACTED:hi-entropy]`;
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
