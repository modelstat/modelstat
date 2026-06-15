/**
 * End-to-end wiring test for the `policies` augment: a server payload is
 * fetched + validated by the loader, compiled, and applied to the in-process
 * floor — proving "the server adds a redaction pattern fleet-wide with no
 * companion release". The floor still applies, and nothing here can weaken it.
 */

import { strict as assert } from "node:assert";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, test } from "node:test";
import { clearRemoteRedactionPatterns, redact } from "@modelstat/core";
import { serveConfig } from "@modelstat/remote-config/testkit";
import { createPolicyRefresher } from "./index.js";

const API = "https://api.test";

afterEach(() => clearRemoteRedactionPatterns());

test("daemon refresher loads a policies payload and augments redaction end-to-end", async () => {
  const fetchImpl = serveConfig({
    policies: {
      version: 4,
      patterns: [{ name: "acme_api_key", regex: "acme_[A-Za-z0-9]{20,}" }],
    },
  });
  const cacheDir = await mkdtemp(join(tmpdir(), "msrc-pol-"));

  // Before: this vendor's token format is not in the bundled floor.
  assert.equal(redact("tok acme_ABCDEFGHIJKLMNOPQRSTUV").counts.secrets_found, 0);

  const refresher = createPolicyRefresher({ apiUrl: API, fetch: fetchImpl, cacheDir });
  await refresher.start();
  await refresher.refresh(); // deterministic network refresh + apply
  refresher.stop();

  // After: the augment is live AND the floor still fires.
  const r = redact(
    "acme_ABCDEFGHIJKLMNOPQRSTUV with sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789",
  );
  assert.match(r.text, /\[REDACTED:acme_api_key\]/);
  assert.match(r.text, /\[REDACTED:anthropic_key\]/);
});

test("offline boot degrades to floor-only (never floor-weakened)", async () => {
  const cacheDir = await mkdtemp(join(tmpdir(), "msrc-pol-"));
  const offline = (async () => {
    throw new Error("offline");
  }) as typeof globalThis.fetch;

  const refresher = createPolicyRefresher({ apiUrl: API, fetch: offline, cacheDir });
  await refresher.start(); // no cache, no network → bundled fallback (empty augment)
  refresher.stop();

  // The floor still redacts real secrets with no augment available.
  const r = redact("sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789");
  assert.match(r.text, /\[REDACTED:anthropic_key\]/);
});
