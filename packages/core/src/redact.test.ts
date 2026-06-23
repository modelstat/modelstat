import { strict as assert } from "node:assert";
import { test } from "node:test";
import { redact } from "./redact.js";

test("redacts anthropic keys", () => {
  const r = redact("use sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789");
  assert.match(r.text, /\[REDACTED:anthropic_key\]/);
  assert.equal(r.counts.secrets_found, 1);
});

test("redacts emails", () => {
  const r = redact("contact: alice@example.com, bob@example.co.uk");
  assert.match(r.text, /\[REDACTED:email\]/);
  assert.equal(r.counts.emails_redacted, 2);
});

test("relativises paths inside repo root", () => {
  const r = redact(
    "see /Users/dev/projects/myrepo/src/foo.ts for details",
    "/Users/dev/projects/myrepo",
  );
  assert.match(r.text, /\.\/src\/foo\.ts/);
  assert.equal(r.counts.paths_redacted_absolute, 0);
});

test("redacts paths outside repo root", () => {
  const r = redact("open /Users/dev/secrets/db.conf", "/Users/dev/projects/myrepo");
  assert.match(r.text, /\[REDACTED:abs-path\]/);
  assert.equal(r.counts.paths_redacted_absolute, 1);
});

test("does not redact normal words or code identifiers", () => {
  const r = redact("function calculateTotalRevenueForQuarter(year, quarter)");
  assert.equal(r.counts.secrets_found, 0);
});

test("collapses hashes / digests", () => {
  // 32-char md5 and 64-char sha256 — both pure hex ≥ 32. (A 40-char hex string
  // is instead caught by the exactly-40 `aws_secret_key` floor rule; either
  // way it is redacted, which is what matters.)
  assert.match(redact("md5: 5d41402abc4b2a76b9719d911017c592").text, /\[REDACTED:hash\]/);
  assert.match(
    redact("sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08").text,
    /\[REDACTED:hash\]/,
  );
});

test("collapses base64 blobs but leaves paths and constants intact", () => {
  assert.match(redact("echo aGVsbG8gd29ybGQgdGhpcyBpcyBhIHRlc3Q=").text, /\[REDACTED:base64\]/);
  // A long relative path uses `/` (a path separator, not a base64 signal) and
  // must survive verbatim — the backend zips script summaries to these tokens.
  const p = redact("tsx packages/daemon-core/src/queue/runner_helper_module");
  assert.match(p.text, /packages\/daemon-core\/src\/queue\/runner_helper_module/);
  // SCREAMING_SNAKE constant names are not secrets.
  const c = redact("export MAX_TOOL_ACTION_PARAM_SHAPE_CHARS_LIMIT");
  assert.match(c.text, /MAX_TOOL_ACTION_PARAM_SHAPE_CHARS_LIMIT/);
});

test("the consolidated floor catches what used to be caught by only one redactor", () => {
  // discord_token / db_url / bearer / modelstat device secret were missing from
  // the wire floor before the two catalogues were unified.
  assert.match(
    redact("Bearer abcdefghijklmnopqrstuvwxyz123456").text,
    /\[REDACTED:bearer_header\]/,
  );
  assert.match(
    redact("postgres://user:hunter2@db.host/app").text,
    /\[REDACTED:db_url_with_password\]/,
  );
  assert.match(
    redact("auth ds_live_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345").text,
    /\[REDACTED:modelstat_device_secret\]/,
  );
});

test("env_secret catches BARE keyword names, not just prefixed ones", () => {
  // Regression: the name pattern's mandatory leading [A-Z] used to eat the first
  // letter, so a bare `SECRET=` / `TOKEN=` / `KEY=` never matched and shipped the
  // value raw. Fixtures below are FICTIONAL — synthetic values that match only the
  // format of the credential shapes this rule must catch (the rule keys on the
  // variable NAME + length, not the literal value).
  for (const [cmd, secret] of [
    ['SECRET="xk_edge_examplefake0000key0000"', "xk_edge_examplefake0000key0000"],
    ['TOKEN="1000000000:examplefake0000tgtoken0000"', "examplefake0000tgtoken0000"],
    ["KEY='phc_examplefake0000posthog0000key'", "phc_examplefake0000posthog0000key"],
    ["PASSWORD=hunter2hunter2hunter2", "hunter2hunter2hunter2"],
    ["export AWS_SECRET_ACCESS_KEY=abcd1234efgh5678ijkl9", "abcd1234efgh5678ijkl9"],
  ] as const) {
    const r = redact(cmd);
    assert.ok(!r.text.includes(secret), `secret must be gone from: ${cmd}`);
    assert.match(r.text, /\[REDACTED:env_secret\]/);
  }
});

test("env_secret does not redact non-secret assignments", () => {
  // Var names without a secret keyword, or short values, must survive — these
  // are useful analytic signal, not secrets.
  for (const cmd of [
    "AWS_PROFILE=dev",
    "CHAIN_ID=1337",
    'export PATH="$HOME/.fly/bin:/opt/homebrew/bin:$PATH"',
    "MONKEY=banana",
  ]) {
    assert.equal(redact(cmd).counts.secrets_found, 0, `should not redact: ${cmd}`);
  }
});
