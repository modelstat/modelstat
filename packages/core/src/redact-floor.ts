/**
 * The privacy floor's secret catalogue — the single, irreducible source of
 * truth for which credential formats get redacted before bytes leave the
 * device.
 *
 * This is the baseline a compromised server can never weaken. It was
 * previously duplicated and *drifting* across `packages/core/src/redact`
 * (the wire floor) and `packages/agent-sdk/src/redact` (the SDK redactor) — one
 * had `discord_token`/`db_url`, the other didn't. They now both source this
 * catalogue, so a newly-leaked credential format is added in exactly one place.
 *
 * Kept deliberately dependency-free (no zod, no imports) so the published,
 * self-contained `@modelstat/agent-sdk` can bundle it without pulling anything
 * else in. The remote `policies` augment (additive-only) layers on top of this
 * floor; it can never replace or disable it.
 *
 * Coverage rule: this catalogue is the UNION of what both redactors caught,
 * with the broader regex chosen wherever they overlapped — so consolidating
 * onto it only ever *strengthens* either redactor, never weakens it.
 */

export interface FloorSecret {
  /** Stable label that appears in the `<REDACTED:name>` placeholder. */
  name: string;
  /** Detection regex. MUST carry the `g` flag — consumers use `String.replace`. */
  pattern: RegExp;
  /**
   * Replacement template (may use `$1`..`$n` backrefs) for consumers that
   * preserve benign context — e.g. keeping the env-var name in
   * `MY_TOKEN=<REDACTED:env_secret>`. The wire floor
   * (`@modelstat/core/redact`) ignores this and replaces the whole match
   * with `[REDACTED:name]`; both are equally safe (the secret is gone).
   */
  replacement: string;
}

/**
 * Ordered specific → generic. Order matters: specific provider keys run before
 * the generic `env_secret` / 40-char-blob catchers so a known key is labelled
 * precisely rather than as a generic blob.
 */
export const SECRET_FLOOR: readonly FloorSecret[] = [
  {
    name: "anthropic_key",
    pattern: /sk-ant-[A-Za-z0-9_-]{20,}/g,
    replacement: "<REDACTED:anthropic_key>",
  },
  {
    name: "openai_key",
    pattern: /sk-(?:proj-)?[A-Za-z0-9_-]{20,}/g,
    replacement: "<REDACTED:openai_key>",
  },
  {
    name: "google_api_key",
    pattern: /AIza[0-9A-Za-z_-]{35}/g,
    replacement: "<REDACTED:google_api_key>",
  },
  {
    name: "aws_access_key",
    pattern: /\b(?:AKIA|ASIA)[0-9A-Z]{16}\b/g,
    replacement: "<REDACTED:aws_access_key>",
  },
  { name: "github_pat", pattern: /ghp_[A-Za-z0-9]{36,}/g, replacement: "<REDACTED:github_pat>" },
  {
    name: "github_oauth",
    pattern: /gho_[A-Za-z0-9]{36,}/g,
    replacement: "<REDACTED:github_oauth>",
  },
  {
    name: "github_app",
    pattern: /gh[sur]_[A-Za-z0-9]{36,}/g,
    replacement: "<REDACTED:github_app>",
  },
  {
    name: "slack_token",
    pattern: /xox[aboprs]-[A-Za-z0-9-]{10,}/g,
    replacement: "<REDACTED:slack_token>",
  },
  {
    name: "stripe_live_key",
    pattern: /(?:sk|pk|rk)_live_[A-Za-z0-9]{24,}/g,
    replacement: "<REDACTED:stripe_live_key>",
  },
  {
    name: "stripe_test_key",
    pattern: /(?:sk|pk|rk)_test_[A-Za-z0-9]{24,}/g,
    replacement: "<REDACTED:stripe_test_key>",
  },
  // Discord bot token (was agent-sdk-only — the canonical drift example).
  {
    name: "discord_token",
    pattern: /[MN][A-Za-z\d]{23}\.[\w-]{6}\.[\w-]{27}/g,
    replacement: "<REDACTED:discord_token>",
  },
  {
    name: "jwt",
    pattern: /eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}/g,
    replacement: "<REDACTED:jwt>",
  },
  // Full PEM block (was header-only in the wire floor — this redacts the body too).
  {
    name: "private_key_header",
    pattern:
      /-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----[\s\S]*?-----END (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----/g,
    replacement: "<REDACTED:private_key>",
  },
  // modelstat's own device bearer — agents must never ship their credential.
  {
    name: "modelstat_device_secret",
    pattern: /ds_live_[A-Za-z0-9_-]{32,}/g,
    replacement: "<REDACTED:modelstat_device_secret>",
  },
  // Generic env-style KEY=VALUE where KEY names a secret. Keeps the var name.
  {
    name: "env_secret",
    pattern:
      /\b([A-Z][A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD|PASSWD|API)[A-Z0-9_]*)\s*[:=]\s*['"]?([^\s'"]{12,})['"]?/g,
    replacement: "$1=<REDACTED:env_secret>",
  },
  {
    name: "bearer_header",
    pattern: /Bearer\s+[A-Za-z0-9._~+/-]{20,}=*/g,
    replacement: "Bearer <REDACTED:bearer>",
  },
  {
    name: "db_url_with_password",
    pattern: /\b(postgres|mysql|mongodb|redis|amqp)(?:\+[a-z]+)?:\/\/[^:\s]+:([^@\s]+)@/gi,
    replacement: "$1://<user>:<REDACTED:db_password>@",
  },
  // Generic 40-char base64-ish blob (e.g. an AWS secret access key on its own).
  // Most generic ⇒ last, so specific patterns claim their matches first.
  {
    name: "aws_secret_key",
    pattern: /(?<![A-Za-z0-9/+=])[A-Za-z0-9/+=]{40}(?![A-Za-z0-9/+=])/g,
    replacement: "<REDACTED:aws_secret_key>",
  },
];
