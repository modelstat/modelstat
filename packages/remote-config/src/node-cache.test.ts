/**
 * Node disk-cache tests. Exercises the real atomic-write layout in a
 * temp dir, proves a read-back bundle still verifies, and runs the
 * daemon-critical path: restart-while-offline serves the last verified
 * bundle from disk, not the bundled baseline.
 */

import assert from "node:assert/strict";
import { mkdtemp, readdir, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { z } from "zod";
import { base64ToBytes } from "./crypto.js";
import { loadRemoteConfig } from "./loader.js";
import { createNodeDiskCache } from "./node.js";
import { generateTestKeypair, signBundle } from "./testkit.js";
import type { CachedBundle, ConfigKind, RemoteConfigEnv } from "./types.js";
import { verifyConfigBytes } from "./verify.js";

const TestPayload = z.object({ version: z.number().int().nonnegative(), value: z.string() });
type TestPayload = z.infer<typeof TestPayload>;

async function tmp(): Promise<string> {
  return mkdtemp(join(tmpdir(), "msrc-"));
}

test("disk cache round-trips a bundle and the read-back still verifies", async () => {
  const dir = await tmp();
  const cache = createNodeDiskCache({ dir });
  const signer = await generateTestKeypair();
  const served = await signBundle({ version: 4, value: "persisted" }, signer);
  const bundle: CachedBundle = {
    version: 4,
    signed_at: served.bundle.signed_at,
    config: served.bundle.config,
    signature: served.bundle.signature,
  };

  await cache.write("policies", bundle);

  // Files land with the documented names.
  const files = (await readdir(dir)).sort();
  assert.deepEqual(files, ["policies.json", "policies.sig", "policies.version"]);

  // policies.json holds the exact signed bytes (human-readable JSON).
  const onDisk = await readFile(join(dir, "policies.json"), "utf8");
  assert.equal(onDisk, '{"version":4,"value":"persisted"}');

  const back = await cache.read("policies");
  assert.ok(back);
  assert.equal(back.version, 4);
  assert.equal(back.signed_at, served.bundle.signed_at);

  // The re-read bytes re-verify under the same key — the property the
  // loader relies on when it trusts (then re-checks) the disk cache.
  const verified = await verifyConfigBytes({
    configBytes: base64ToBytes(back.config),
    signature: base64ToBytes(back.signature),
    publicKey: signer.publicKey,
    schema: TestPayload,
    expectedVersion: 4,
  });
  assert.equal(verified.ok, true);
});

test("missing / torn cache reads back as null (never throws)", async () => {
  const dir = await tmp();
  const cache = createNodeDiskCache({ dir });
  assert.equal(await cache.read("policies"), null);

  // A partial triple (only .json present) is also "no cache".
  await writeFile(join(dir, "half.json"), "{}");
  assert.equal(await cache.read("half"), null);
});

test("daemon restart-while-offline serves the last verified bundle from disk", async () => {
  const dir = await tmp();
  const cache = createNodeDiskCache({ dir });
  const signer = await generateTestKeypair();

  // Simulate a previous online session having persisted v7.
  const served = await signBundle({ version: 7, value: "last-good" }, signer);
  await cache.write("testkind", {
    version: 7,
    signed_at: served.bundle.signed_at,
    config: served.bundle.config,
    signature: served.bundle.signature,
  });

  // Now boot offline: the manifest fetch throws.
  const kind: ConfigKind<TestPayload> = {
    kind: "testkind",
    schema: TestPayload,
    bundledFallback: { version: 1, value: "bundled" },
  };
  const env: RemoteConfigEnv = {
    apiUrl: "https://api.test",
    publicKey: signer.publicKey,
    fetch: (async () => {
      throw new Error("offline");
    }) as typeof fetch,
    cache,
  };

  const result = await loadRemoteConfig(kind, env);
  assert.equal(result.source, "disk");
  assert.equal(result.version, 7);
  assert.equal(result.value.value, "last-good");
});
