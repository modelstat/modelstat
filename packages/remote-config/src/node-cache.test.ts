/**
 * Node disk-cache tests. Exercises the real atomic-write layout in a temp
 * dir, and runs the daemon-critical path: restart-while-offline serves the
 * last good payload from disk, not the bundled baseline.
 */

import assert from "node:assert/strict";
import { mkdtemp, readdir, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { z } from "zod";
import { loadRemoteConfig } from "./loader.js";
import { createNodeDiskCache } from "./node.js";
import type { ConfigKind, RemoteConfigEnv } from "./types.js";

const TestPayload = z.object({ version: z.number().int().nonnegative(), value: z.string() });
type TestPayload = z.infer<typeof TestPayload>;

async function tmp(): Promise<string> {
  return mkdtemp(join(tmpdir(), "msrc-"));
}

test("disk cache round-trips a payload", async () => {
  const dir = await tmp();
  const cache = createNodeDiskCache({ dir });

  await cache.write("policies", { version: 4, value: "persisted" });

  // One human-readable JSON file per kind.
  const files = (await readdir(dir)).sort();
  assert.deepEqual(files, ["policies.json"]);
  const onDisk = await readFile(join(dir, "policies.json"), "utf8");
  assert.equal(onDisk.trim(), '{"version":4,"value":"persisted"}');

  const back = await cache.read("policies");
  assert.deepEqual(back, { version: 4, value: "persisted" });
});

test("missing / torn cache reads back as null (never throws)", async () => {
  const dir = await tmp();
  const cache = createNodeDiskCache({ dir });
  assert.equal(await cache.read("policies"), null);

  // A non-JSON file is also "no cache" — the loader re-validates anyway.
  await writeFile(join(dir, "torn.json"), "{ not json");
  assert.equal(await cache.read("torn"), null);
});

test("daemon restart-while-offline serves the last good payload from disk", async () => {
  const dir = await tmp();
  const cache = createNodeDiskCache({ dir });

  // Simulate a previous online session having persisted v7.
  await cache.write("testkind", { version: 7, value: "last-good" });

  // Now boot offline: the fetch throws.
  const kind: ConfigKind<TestPayload> = {
    kind: "testkind",
    schema: TestPayload,
    bundledFallback: { version: 1, value: "bundled" },
  };
  const env: RemoteConfigEnv = {
    apiUrl: "https://api.test",
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
