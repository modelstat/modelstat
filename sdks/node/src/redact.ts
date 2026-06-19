/**
 * The privacy floor: deterministic, dependency-light redaction that runs
 * **in-process before any bytes leave the SDK**.
 *
 * This is the TypeScript port of the Rust SDK's `redact.rs` (itself a port of
 * the daemon's `SECRET_FLOOR` plus the email / absolute-path PII rules). It is
 * the irreducible baseline — even in "raw" remote mode the floor still scrubs
 * live credentials; "raw" means *full turns*, not *leaked keys*.
 *
 * Placeholder style is SQUARE brackets `[REDACTED:name]` (matching the Rust
 * SDK — NOT the daemon's angle brackets). Rules are ordered specific → generic
 * so a known provider key is labelled precisely before the generic
 * env-secret / blob catchers run. Every pattern uses the `g` flag.
 *
 * Unlike Rust's `regex` crate, JS regex supports look-around, so the one
 * boundary-sensitive pattern (the 40-char AWS-secret blob) uses a clean
 * lookbehind/lookahead pair (Node ≥20) rather than explicit boundary capture
 * groups.
 */

/** Result of a redaction pass. */
export interface Redacted {
  /** The cleaned text. */
  text: string;
  /** Count of secret-format matches replaced. */
  secrets: number;
  /** Count of PII matches replaced (emails, absolute paths). */
  pii: number;
}

interface Rule {
  re: RegExp;
  /** Replacement string (may use `$1`/`$2` capture references). */
  replacement: string;
}

/**
 * Ordered specific → generic. Specific provider keys run before the generic
 * env-secret / blob catchers so a known key is labelled precisely.
 *
 * Each `RegExp` carries the `g` flag so `replace` swaps every occurrence and
 * `match` counts every occurrence. Patterns are recreated lazily on first use
 * (a module-level const array is fine — `lastIndex` is only consulted by
 * stateful `.exec`/`.test`, and we use `.match`/`.replace`, which reset it).
 */
const FLOOR: Rule[] = [
  // 1. Anthropic API keys.
  { re: /sk-ant-[A-Za-z0-9_-]{20,}/g, replacement: "[REDACTED:anthropic_key]" },
  // 2. OpenAI keys (incl. project-scoped `sk-proj-`).
  { re: /sk-(?:proj-)?[A-Za-z0-9_-]{20,}/g, replacement: "[REDACTED:openai_key]" },
  // 3. Google API keys.
  { re: /AIza[0-9A-Za-z_-]{35}/g, replacement: "[REDACTED:google_api_key]" },
  // 4. AWS access key ids.
  { re: /\b(?:AKIA|ASIA)[0-9A-Z]{16}\b/g, replacement: "[REDACTED:aws_access_key]" },
  // 5-7. GitHub PAT / OAuth / app tokens.
  { re: /ghp_[A-Za-z0-9]{36,}/g, replacement: "[REDACTED:github_pat]" },
  { re: /gho_[A-Za-z0-9]{36,}/g, replacement: "[REDACTED:github_oauth]" },
  { re: /gh[sur]_[A-Za-z0-9]{36,}/g, replacement: "[REDACTED:github_app]" },
  // 8. Slack tokens.
  { re: /xox[aboprs]-[A-Za-z0-9-]{10,}/g, replacement: "[REDACTED:slack_token]" },
  // 9-10. Stripe live / test keys.
  { re: /(?:sk|pk|rk)_live_[A-Za-z0-9]{24,}/g, replacement: "[REDACTED:stripe_live_key]" },
  { re: /(?:sk|pk|rk)_test_[A-Za-z0-9]{24,}/g, replacement: "[REDACTED:stripe_test_key]" },
  // 11. Discord bot tokens.
  { re: /[MN][A-Za-z\d]{23}\.[\w-]{6}\.[\w-]{27}/g, replacement: "[REDACTED:discord_token]" },
  // 12. JWTs (three base64url segments).
  {
    re: /eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}/g,
    replacement: "[REDACTED:jwt]",
  },
  // 13. PEM private-key blocks (multi-line; non-greedy body).
  {
    re: /-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----[\s\S]*?-----END (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----/g,
    replacement: "[REDACTED:private_key]",
  },
  // 14. modelstat device secrets.
  { re: /ds_live_[A-Za-z0-9_-]{32,}/g, replacement: "[REDACTED:modelstat_device_secret]" },
  // 15. Generic env-style KEY=VALUE where KEY names a secret. KEEP the var name.
  {
    re: /\b([A-Z][A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD|PASSWD|API)[A-Z0-9_]*)\s*[:=]\s*['"]?([^\s'"]{12,})['"]?/g,
    replacement: "$1=[REDACTED:env_secret]",
  },
  // 16. Bearer tokens.
  { re: /Bearer\s+[A-Za-z0-9._~+/-]{20,}=*/g, replacement: "Bearer [REDACTED:bearer]" },
  // 17. DB connection strings — drop the password, keep scheme + a `<user>` stub.
  {
    re: /\b(postgres|mysql|mongodb|redis|amqp)(?:\+[a-z]+)?:\/\/[^:\s]+:([^@\s]+)@/gi,
    replacement: "$1://<user>:[REDACTED:db_password]@",
  },
  // 18. (most generic, LAST among secrets) A lone 40-char base64-ish blob, e.g.
  //     an AWS secret access key. Look-around keeps an embedded blob inside a
  //     longer token untouched.
  {
    re: /(?<![A-Za-z0-9/+=])[A-Za-z0-9/+=]{40}(?![A-Za-z0-9/+=])/g,
    replacement: "[REDACTED:aws_secret_key]",
  },
];

// --- PII rules (after the secret floor) --------------------------------------

const EMAIL = /[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}/g;

/**
 * Absolute home paths on macOS / Linux / Windows — they leak usernames and
 * machine layout.
 */
const ABS_PATH = /(?:\/Users\/|\/home\/)[^\s"'`)]+|[A-Za-z]:\\Users\\[^\s"'`)]+/g;

/** Count how many times `re` (a `g`-flagged pattern) matches `text`. */
function countMatches(text: string, re: RegExp): number {
  const m = text.match(re);
  return m ? m.length : 0;
}

/**
 * Redact `input` against the floor. Returns the cleaned text and per-class
 * counts (counts are taken *before* the replacement, matching the Rust
 * reference). When nothing matches, the input is returned unchanged.
 */
export function redact(input: string): Redacted {
  let text = input;
  let secrets = 0;
  let pii = 0;

  for (const rule of FLOOR) {
    const n = countMatches(text, rule.re);
    if (n > 0) {
      text = text.replace(rule.re, rule.replacement);
      secrets += n;
    }
  }

  const emails = countMatches(text, EMAIL);
  if (emails > 0) {
    text = text.replace(EMAIL, "[REDACTED:email]");
    pii += emails;
  }

  const paths = countMatches(text, ABS_PATH);
  if (paths > 0) {
    text = text.replace(ABS_PATH, "[REDACTED:path]");
    pii += paths;
  }

  return { text, secrets, pii };
}
