/**
 * End-to-end wiring test for the signed `policies` augment:
 * a server-signed bundle is fetched + verified by the loader, compiled, and
 * applied to the in-process floor — proving "the server adds a redaction
 * pattern fleet-wide with no companion release". The floor still applies, and
 * nothing here can weaken it.
 */

import { strict as assert } from "node:assert";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, test } from "node:test";
import { clearRemoteRedactionPatterns, redact } from "@modelstat/core";
import { generateTestKeypair, manifestFor, signBundle } from "@modelstat/remote-config/testkit";
import { createPolicyRefresher } from "./index.js";

const API = "https://api.test";

afterEach(() => clearRemoteRedactionPatterns());

test("daemon refresher loads a signed policies bundle and augments redaction end-to-end", async () => {
  const signer = await generateTestKeypair();
  const payload = {
    version: 4,
    patterns: [{ name: "acme_api_key", regex: "acme_[A-Za-z0-9]{20,}" }],
  };
  const served = await signBundle(payload, signer);
  const bundleUrl = "/v1/config/policies/bundle/4.json";
  const manifest = await manifestFor("policies", served, bundleUrl);

  const fetchImpl = (async (input: string | URL | Request): Promise<Response> => {
    const url = typeof input === "string" ? input : input.toString();
    if (url === `${API}/v1/config/policies/manifest.json`) {
      return new Response(JSON.stringify(manifest), { status: 200 });
    }
    if (url === `${API}${bundleUrl}`) return new Response(served.bytes, { status: 200 });
    return new Response("404", { status: 404 });
  }) as typeof globalThis.fetch;

  const cacheDir = await mkdtemp(join(tmpdir(), "msrc-pol-"));

  // Before: this vendor's token format is not in the bundled floor.
  assert.equal(redact("tok acme_ABCDEFGHIJKLMNOPQRSTUV").counts.secrets_found, 0);

  const refresher = createPolicyRefresher({
    apiUrl: API,
    publicKey: signer.publicKey,
    fetch: fetchImpl,
    cacheDir,
  });
  await refresher.start();
  await refresher.refresh(); // deterministic network refresh + apply
  refresher.stop();

  // After: the signed augment is live AND the floor still fires.
  const r = redact(
    "acme_ABCDEFGHIJKLMNOPQRSTUV with sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789",
  );
  assert.match(r.text, /\[REDACTED:acme_api_key\]/);
  assert.match(r.text, /\[REDACTED:anthropic_key\]/);
});

test("offline boot degrades to floor-only (never floor-weakened)", async () => {
  const signer = await generateTestKeypair();
  const cacheDir = await mkdtemp(join(tmpdir(), "msrc-pol-"));
  const offline = (async () => {
    throw new Error("offline");
  }) as typeof globalThis.fetch;

  const refresher = createPolicyRefresher({
    apiUrl: API,
    publicKey: signer.publicKey,
    fetch: offline,
    cacheDir,
  });
  await refresher.start(); // no cache, no network → bundled fallback (empty augment)
  refresher.stop();

  // The floor still redacts real secrets with no augment available.
  const r = redact("sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789");
  assert.match(r.text, /\[REDACTED:anthropic_key\]/);
});
