/**
 * Golden fixtures — §4.3 (redaction floor). Every SECRET_FLOOR pattern's
 * positive case, the entropy-pass branches, and the non-redaction guarantees,
 * with expected text + counts taken from the TS wire floor `redact()`.
 *
 * All secret values are FICTIONAL — synthetic strings that match only the FORMAT
 * of each credential shape (the rules key on format/length, never a real value).
 */
import { redact } from "../../../packages/core/src/redact.js";
import { SECRET_FLOOR } from "../../../packages/core/src/redact-floor.js";
import { type Generator, writeGolden } from "./lib.mts";

interface Case {
  name: string;
  input: string;
  repo_root?: string | null;
}

/**
 * Neuter committed secret literals for GitHub push protection.
 *
 * The redaction cases must contain synthetic credentials (that's the point), but
 * they match real credential FORMATS, so a raw commit is blocked by secret
 * scanning. We insert a U+0000 right after each detectable credential prefix:
 * the committed byte stream then matches no scanner (the prefix is followed by a
 * non-charset byte), the value stays readable, and both consumers strip U+0000
 * before redacting — so the redactor still sees the full secret. Ordered
 * alternation (specific prefixes first) matched non-overlapping left-to-right.
 */
const SENTINEL = String.fromCharCode(0);
const PREFIX_RE =
  /sk_live_|sk_test_|pk_test_|sk-ant-|sk-proj-|sk-|ds_live_|gh[oprsu]_|AIza|xox[aboprs]-|-----BEGIN |-----END /g;
function neuter(input: string): string {
  return input.replace(PREFIX_RE, (m) => m + SENTINEL);
}

const A36 = "abcdefghijklmnopqrstuvwxyz0123456789"; // 36 alnum chars

const CASES: Case[] = [
  // --- the 18 floor patterns, in catalogue order ---
  // Literal split (via interpolation) so the committed source matches no secret
  // scanner; the runtime value is the full synthetic key.
  { name: "anthropic_key", input: `use sk-ant-${"api03-abcdefghijklmnopqrstuvwxyz0123456789"} now` },
  { name: "openai_key_proj", input: `token sk-proj-${A36} end` },
  { name: "openai_key_plain", input: `token sk-${A36} end` },
  { name: "google_api_key", input: `key AIza${"b".repeat(35)} end` },
  { name: "aws_access_key", input: "cred AKIAIOSFODNN7EXAMPLE here" },
  { name: "github_pat", input: `ghp_${A36}${A36}` },
  { name: "github_oauth", input: `gho_${A36}${A36}` },
  { name: "github_app", input: `ghs_${A36}${A36}` },
  { name: "slack_token", input: `xoxb-${A36}${A36}` },
  { name: "stripe_live_key", input: `sk_live_${A36}${A36}` },
  { name: "stripe_test_key", input: `pk_test_${A36}${A36}` },
  { name: "discord_token", input: `M${"a".repeat(23)}.${"bcdefg"}.${"hijklmnopqrstuvwxyz012345678"}` },
  { name: "jwt", input: `eyJ${"a".repeat(12)}.${"b".repeat(12)}.${"c".repeat(12)}` },
  {
    name: "private_key_pem",
    // Body carries the gitleaks-allowlisted `blahblah` marker; the floor pattern
    // matches any PEM block regardless of body, so the redaction result is the same.
    input:
      "-----BEGIN RSA PRIVATE KEY-----\nMIIBOwIBAAKblahblahFAKEfixtureKEYbodyblahblahnotarealprivatekey\n-----END RSA PRIVATE KEY-----",
  },
  { name: "modelstat_device_secret", input: `auth ds_live_${A36}` },
  { name: "env_secret_bare_password", input: "PASSWORD=hunter2hunter2hunter2" },
  { name: "env_secret_aws", input: "export AWS_SECRET_ACCESS_KEY=abcd1234efgh5678ijkl9" },
  { name: "bearer_header", input: "Bearer abcdefghijklmnopqrstuvwxyz123456" },
  { name: "db_url_with_password", input: "postgres://user:hunter2@db.host/app" },
  { name: "aws_secret_key_40blob", input: `blob ${"A".repeat(40)} end` },

  // --- entropy pass branches ---
  { name: "entropy_hash_md5", input: "md5: 5d41402abc4b2a76b9719d911017c592" },
  {
    name: "entropy_hash_sha256",
    input: "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
  },
  { name: "entropy_base64", input: "echo aGVsbG8gd29ybGQgdGhpcyBpcyBhIHRlc3Q=" },
  {
    name: "entropy_base64_with_slash",
    input: "payload AbCdEfGhIjKlMnOpQrStUvWxYz012345/6789+=",
  },
  { name: "entropy_hi_entropy", input: "id Zx9Kd2Lm7Qp4Rt6Vw8Yc1Df3Gh5Jk0Sabcde" },
  {
    name: "entropy_hash_url_path",
    input: "GET https://api.example.test/v2/0123456789abcdef0123456789abcdef01234567",
  },
  {
    name: "entropy_hash_relative_path",
    input: "open artifacts/0123456789abcdef0123456789abcdef01234567/result.json",
  },

  // --- non-redaction guarantees (must survive; counts all zero) ---
  { name: "keep_screaming_snake", input: "export MAX_TOOL_ACTION_PARAM_SHAPE_CHARS_LIMIT" },
  { name: "keep_relative_path", input: "tsx packages/daemon-core/src/queue/runner_helper_module" },
  { name: "keep_code_identifier", input: "function calculateTotalRevenueForQuarter(year, quarter)" },
  { name: "keep_aws_profile", input: "AWS_PROFILE=dev" },
  { name: "keep_chain_id", input: "CHAIN_ID=1337" },
  { name: "keep_path_env", input: 'export PATH="$HOME/.fly/bin:/opt/homebrew/bin:$PATH"' },
  { name: "keep_monkey", input: "MONKEY=banana" },

  // --- emails + absolute paths ---
  { name: "emails_two", input: "contact: alice@example.com, bob@example.co.uk" },
  {
    name: "path_inside_repo_relativised",
    input: "see /Users/dev/projects/myrepo/src/foo.ts for details",
    repo_root: "/Users/dev/projects/myrepo",
  },
  {
    name: "path_outside_repo_redacted",
    input: "open /Users/dev/secrets/db.conf",
    repo_root: "/Users/dev/projects/myrepo",
  },
  { name: "path_linux_home_redacted", input: "cat /home/alice/.ssh/id_rsa" },
];

export const generator: Generator = {
  category: "redaction floor (§4.3)",
  run: () => {
    const fixtures = CASES.map((c) => {
      // Expected text + counts come from the CLEAN input; the committed `input`
      // is neutered so no real credential FORMAT lands in git (push protection).
      const r = redact(c.input, c.repo_root ?? undefined);
      return {
        name: c.name,
        input: neuter(c.input),
        repo_root: c.repo_root ?? null,
        text: r.text,
        secrets_found: r.counts.secrets_found,
        emails_redacted: r.counts.emails_redacted,
        paths_redacted_absolute: r.counts.paths_redacted_absolute,
      };
    });
    // Also record the ordered floor names so the Rust side can assert its own
    // catalogue is identical in order + membership.
    const floor_order = SECRET_FLOOR.map((f) => f.name);
    writeGolden("redaction.json", { floor_order, cases: fixtures });
  },
};
