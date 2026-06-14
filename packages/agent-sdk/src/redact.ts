/**
 * Client-side redaction for AI session data, applied BEFORE bytes leave
 * the user's machine. Agents that care about latency, network, and data
 * minimisation should always pre-redact rather than relying on anything
 * downstream.
 *
 * Policies are versioned. Every redacted session carries a `processing`
 * record (see ProcessingMetadata in @modelstat/core) that names the
 * policy + version + redaction count, so the server / dashboard can
 * always show what was processed and by whom.
 *
 * Available policies:
 *   none           — pass-through, no redaction (only with explicit opt-in)
 *   secrets-only   — strip API keys, JWTs, AWS keys, env-style KEY=VALUE
 *   strict-pii-v2  — secrets + PII (emails, phones, IPs, paths, hostnames)
 *   paranoid       — strict-pii + drop ALL stdout/stderr blobs
 */

export type PolicyName = "none" | "secrets-only" | "strict-pii-v2" | "paranoid";

export const POLICY_VERSIONS: Record<PolicyName, string> = {
  none: "1",
  "secrets-only": "1",
  "strict-pii-v2": "2",
  paranoid: "1",
};

export type RedactionResult<T> = {
  data: T;
  /** Number of distinct redactions applied across the input. */
  redactionsApplied: number;
  /** The policy + version that was applied (for provenance metadata). */
  policy: PolicyName;
  policyVersion: string;
};

/* ─── Pattern catalogue ────────────────────────────────────────────── */

// Each pattern: regex + replacement label. Order matters — the more
// specific patterns run first so generic catch-alls don't shadow them.
type Pattern = {
  name: string;
  re: RegExp;
  /** Replacement; can use $1 backrefs. */
  with: string;
};

const SECRETS: readonly Pattern[] = [
  // Anthropic / OpenAI / common provider keys.
  { name: "anthropic_api_key", re: /sk-ant-[A-Za-z0-9_-]{32,}/g, with: "<REDACTED:anthropic_api_key>" },
  { name: "openai_api_key", re: /sk-(?:proj-)?[A-Za-z0-9_-]{32,}/g, with: "<REDACTED:openai_api_key>" },
  { name: "github_token", re: /(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}/g, with: "<REDACTED:github_token>" },
  { name: "stripe_key", re: /(?:sk|pk|rk)_(?:test|live)_[A-Za-z0-9]{24,}/g, with: "<REDACTED:stripe_key>" },
  { name: "aws_access_key", re: /\b(?:AKIA|ASIA)[0-9A-Z]{16}\b/g, with: "<REDACTED:aws_access_key>" },
  // 40-char base64-ish AWS secret often appears alongside an access key.
  { name: "aws_secret_key", re: /\baws_secret_access_key\s*[:=]\s*['"]?([A-Za-z0-9/+=]{40})['"]?/gi, with: "aws_secret_access_key=<REDACTED:aws_secret_key>" },
  // Slack / Discord / Telegram bot tokens.
  { name: "slack_token", re: /xox[abposr]-[A-Za-z0-9-]{10,}/g, with: "<REDACTED:slack_token>" },
  { name: "discord_token", re: /[MN][A-Za-z\d]{23}\.[\w-]{6}\.[\w-]{27}/g, with: "<REDACTED:discord_token>" },
  // Generic JWTs.
  { name: "jwt", re: /eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}/g, with: "<REDACTED:jwt>" },
  // Generic env-style KEY=VALUE where KEY contains TOKEN/KEY/SECRET/PASSWORD.
  // Be conservative: require a quoted or =-bound value of at least 12 chars.
  { name: "env_secret", re: /\b([A-Z][A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD|PASSWD|API)[A-Z0-9_]*)\s*[:=]\s*['"]?([^\s'"]{12,})['"]?/g, with: "$1=<REDACTED:env_secret>" },
  // Private keys (PEM blocks).
  { name: "pem_private_key", re: /-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----[\s\S]*?-----END (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----/g, with: "<REDACTED:private_key_pem>" },
  // Bearer headers.
  { name: "bearer_header", re: /Bearer\s+[A-Za-z0-9._~+/-]{20,}=*/g, with: "Bearer <REDACTED:bearer>" },
  // Database connection strings with passwords.
  { name: "db_url_with_password", re: /\b(postgres|mysql|mongodb|redis|amqp)(?:\+[a-z]+)?:\/\/[^:\s]+:([^@\s]+)@/gi, with: "$1://<user>:<REDACTED:db_password>@" },
];

// Recognise modelstat's own device_secret format too — agents shouldn't
// ship their auth credential into a session log.
const MODELSTAT_SECRETS: readonly Pattern[] = [
  { name: "modelstat_device_secret", re: /ds_live_[A-Za-z0-9_-]{32,}/g, with: "<REDACTED:modelstat_device_secret>" },
];

const PII: readonly Pattern[] = [
  // Email — RFC 5322-lite. Catches almost every realistic case.
  { name: "email", re: /\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b/g, with: "<REDACTED:email>" },
  // Phone numbers (E.164-ish + common formats).
  { name: "phone", re: /(?:\+?\d{1,3}[\s.-]?)?(?:\(?\d{3}\)?[\s.-]?)\d{3}[\s.-]?\d{4}\b/g, with: "<REDACTED:phone>" },
  // IPv4 (skip private and loopback ranges via post-filter — see redactPii).
  // We intentionally don't redact IPv6 by default; the false-positive
  // rate against UUIDs and hashes is too high.
  { name: "ipv4", re: /\b(?:(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\b/g, with: "<REDACTED:ipv4>" },
  // Credentials in URLs.
  { name: "url_credentials", re: /(https?:\/\/)([^:/\s]+):([^@\s]+)@/g, with: "$1<user>:<REDACTED:url_password>@" },
  // macOS user paths.
  { name: "mac_user_path", re: /\/Users\/[^/\s'"]+/g, with: "<HOME>" },
  // Linux user paths.
  { name: "linux_user_path", re: /\/home\/[^/\s'"]+/g, with: "<HOME>" },
  // Windows user paths.
  { name: "win_user_path", re: /[A-Z]:\\Users\\[^\\\s'"]+/gi, with: "<HOME>" },
];

/* ─── Helpers ─────────────────────────────────────────────────────── */

function isPrivateOrLocalIp(ip: string): boolean {
  const parts = ip.split(".").map(Number);
  if (parts.length !== 4 || parts.some((n) => Number.isNaN(n))) return false;
  const [a = 0, b = 0] = parts;
  if (a === 10) return true;
  if (a === 127) return true;
  if (a === 169 && b === 254) return true;
  if (a === 172 && b >= 16 && b <= 31) return true;
  if (a === 192 && b === 168) return true;
  if (a === 0) return true;
  return false;
}

function applyPatterns(s: string, patterns: readonly Pattern[]): { out: string; count: number } {
  let out = s;
  let count = 0;
  for (const p of patterns) {
    if (p.name === "ipv4") {
      // Skip private/loopback/link-local by default.
      out = out.replace(p.re, (match) => {
        if (isPrivateOrLocalIp(match)) return match;
        count++;
        return p.with;
      });
    } else {
      out = out.replace(p.re, (...args) => {
        count++;
        // Reconstruct the replacement with backrefs.
        const lastArg = args[args.length - 1];
        const groups = typeof lastArg === "string" ? args.slice(1, -2) : args.slice(1, -1);
        return p.with.replace(/\$(\d+)/g, (_, n: string) => {
          const i = Number(n) - 1;
          return typeof groups[i] === "string" ? (groups[i] as string) : "";
        });
      });
    }
  }
  return { out, count };
}

/* ─── Public API ──────────────────────────────────────────────────── */

/**
 * Recursively redact every string field of `input` according to `policy`.
 * Returns a deep clone — the input is never mutated.
 *
 * @example
 *   const { data, redactionsApplied } = redact(session, "strict-pii-v2");
 */
export function redact<T>(input: T, policy: PolicyName = "strict-pii-v2"): RedactionResult<T> {
  if (policy === "none") {
    return {
      data: input,
      redactionsApplied: 0,
      policy,
      policyVersion: POLICY_VERSIONS.none,
    };
  }

  const patterns: Pattern[] = [...MODELSTAT_SECRETS, ...SECRETS];
  if (policy === "strict-pii-v2" || policy === "paranoid") {
    patterns.push(...PII);
  }

  let total = 0;
  const visit = (v: unknown): unknown => {
    if (typeof v === "string") {
      const { out, count } = applyPatterns(v, patterns);
      total += count;
      return out;
    }
    if (Array.isArray(v)) return v.map(visit);
    if (v && typeof v === "object") {
      const out: Record<string, unknown> = {};
      for (const [k, val] of Object.entries(v)) {
        // Paranoid mode: drop entire stdout/stderr/output blob fields.
        if (policy === "paranoid" && /^(stdout|stderr|output|raw_text|tool_output|response_text)$/i.test(k)) {
          out[k] = "<REDACTED:dropped_blob>";
          total++;
          continue;
        }
        out[k] = visit(val);
      }
      return out;
    }
    return v;
  };

  return {
    data: visit(input) as T,
    redactionsApplied: total,
    policy,
    policyVersion: POLICY_VERSIONS[policy],
  };
}

/** Convenience: serialise the result of redact() into the
 * `processing` block that goes alongside an upload. */
export function processingFor(
  result: { policy: PolicyName; policyVersion: string; redactionsApplied: number },
  agentId: string,
  originalSizeBytes: number,
  uploadedSizeBytes: number,
): {
  redacted_by: string;
  redaction_policy: string;
  redaction_policy_version: string;
  redactions_applied: number;
  original_size_bytes: number;
  uploaded_size_bytes: number;
} {
  return {
    redacted_by: agentId,
    redaction_policy: result.policy,
    redaction_policy_version: result.policyVersion,
    redactions_applied: result.redactionsApplied,
    original_size_bytes: originalSizeBytes,
    uploaded_size_bytes: uploadedSizeBytes,
  };
}
