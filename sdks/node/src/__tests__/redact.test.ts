import assert from "node:assert/strict";
import test from "node:test";
import { redact } from "../index.js";

const clean = (s: string): string => redact(s).text;

test("scrubs each secret family", () => {
  const cases: Array<[string, string]> = [
    ["sk-ant-0123456789abcdefghijABCDEF", "anthropic_key"],
    ["sk-proj-0123456789abcdefghijABCDEF", "openai_key"],
    ["AIzaSyA1234567890123456789012345678901234", "google_api_key"],
    ["AKIAIOSFODNN7EXAMPLE", "aws_access_key"],
    ["ghp_0123456789012345678901234567890123456789", "github_pat"],
    ["gho_0123456789012345678901234567890123456789", "github_oauth"],
    ["ghs_0123456789012345678901234567890123456789", "github_app"],
    ["xoxb-1234567890-abcdefghijkl", "slack_token"],
    ["sk_live_0123456789012345678901234567", "stripe_live_key"],
    ["sk_test_0123456789012345678901234567", "stripe_test_key"],
    ["ds_live_0123456789012345678901234567890123", "modelstat_device_secret"],
  ];
  for (const [input, label] of cases) {
    const out = clean(input);
    assert.ok(
      out.includes(`[REDACTED:${label}]`),
      `expected [REDACTED:${label}] for ${JSON.stringify(input)}, got ${JSON.stringify(out)}`,
    );
    assert.ok(!out.includes(input), `raw secret leaked: ${out}`);
  }
});

test("redacts a JWT", () => {
  const jwt =
    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
  const out = clean(`token=${jwt}`);
  assert.ok(out.includes("[REDACTED:jwt]"), out);
  assert.ok(!out.includes(jwt));
});

test("redacts a Discord bot token", () => {
  // Shape: [MN] + 23 chars, ".", 6 chars, ".", 27 chars.
  const tok = `M${"a".repeat(23)}.${"b".repeat(6)}.${"c".repeat(27)}`;
  const out = clean(tok);
  assert.ok(out.includes("[REDACTED:discord_token]"), out);
  assert.ok(!out.includes(tok));
});

test("redacts a PEM private key block", () => {
  const pem =
    "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nblahblah\n-----END RSA PRIVATE KEY-----";
  const out = clean(`here ${pem} done`);
  assert.ok(out.includes("[REDACTED:private_key]"), out);
  assert.ok(!out.includes("MIIEowIBAAKCAQEA"));
});

test("keeps env-var name but drops the value", () => {
  const out = clean("MY_API_TOKEN=supersecretvalue123");
  assert.ok(out.includes("MY_API_TOKEN="), out);
  assert.ok(out.includes("[REDACTED:env_secret]"), out);
  assert.ok(!out.includes("supersecretvalue123"));
});

test("redacts Bearer tokens and DB passwords", () => {
  const b = clean("Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123");
  assert.ok(b.includes("Bearer [REDACTED:bearer]"), b);

  const d = clean("postgres://app:hunter2hunter2@db.internal:5432/prod");
  assert.ok(d.includes("[REDACTED:db_password]"), d);
  assert.ok(d.includes("postgres://<user>:"), d);
  assert.ok(!d.includes("hunter2hunter2"));
});

test("redacts email and absolute paths as PII (count == 2)", () => {
  const r = redact(
    "ping me at jane.doe@example.com from /Users/jane/secret/app.ts",
  );
  assert.ok(r.text.includes("[REDACTED:email]"), r.text);
  assert.ok(r.text.includes("[REDACTED:path]"), r.text);
  assert.equal(r.pii, 2);
});

test("redacts a /home/ absolute path", () => {
  const r = redact("logfile at /home/alice/.config/app/log.txt please");
  assert.ok(r.text.includes("[REDACTED:path]"), r.text);
  assert.ok(!r.text.includes("/home/alice"));
});

test("leaves clean text untouched and counts zero", () => {
  const r = redact("refactor the auth module and add tests");
  assert.equal(r.text, "refactor the auth module and add tests");
  assert.equal(r.secrets, 0);
  assert.equal(r.pii, 0);
});

test("catches a lone 40-char AWS secret blob (lookbehind/lookahead)", () => {
  const key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
  assert.equal(key.length, 40);
  const out = clean(`aws_secret = ${key}`);
  assert.ok(out.includes("[REDACTED:aws_secret_key]"), out);
  assert.ok(!out.includes(key));
});

test("does NOT redact a blob embedded inside a longer token", () => {
  // 80 contiguous base64-ish chars => no bounded 40-run => left alone.
  const longer = "x".repeat(80);
  const out = clean(longer);
  assert.equal(out, longer);
});
