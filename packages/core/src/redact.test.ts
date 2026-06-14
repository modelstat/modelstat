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

test("the consolidated floor catches what used to be agent-sdk-only", () => {
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
