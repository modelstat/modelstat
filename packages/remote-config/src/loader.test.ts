/**
 * Loader + fallback-ladder tests. Everything runs against an ephemeral
 * Ed25519 keypair (testkit) and a routed mock `fetch`, so no network and
 * no real key are involved. These are the Phase-0 acceptance checks:
 * a signed bundle round-trips; bad signature / sha / key / version are
 * rejected; offline → disk → bundled is proven; failure never clobbers.
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import { z } from "zod";
import { loadRemoteConfig, RemoteConfigStore } from "./loader.js";
import { generateTestKeypair, manifestFor, signBundle, type TestSigner } from "./testkit.js";
import type { CachedBundle, CacheStore, ConfigKind, RemoteConfigEnv } from "./types.js";

const API = "https://api.test";

const TestPayload = z.object({
  version: z.number().int().nonnegative(),
  value: z.string(),
});
type TestPayload = z.infer<typeof TestPayload>;

const BUNDLED: TestPayload = { version: 1, value: "bundled" };

function testKind(): ConfigKind<TestPayload> {
  return { kind: "testkind", schema: TestPayload, bundledFallback: BUNDLED };
}

function manifestUrl(): string {
  return `${API}/v1/config/testkind/manifest.json`;
}

/** A routed mock fetch. Each route is a thunk so a test can assert a
 * route is never hit (by throwing) or count calls. */
function mockFetch(routes: Record<string, () => Response>): {
  fetch: typeof fetch;
  calls: string[];
} {
  const calls: string[] = [];
  const fn = (async (input: string | URL | Request): Promise<Response> => {
    const url = typeof input === "string" ? input : input.toString();
    calls.push(url);
    const make = routes[url];
    if (!make) return new Response("not found", { status: 404 });
    return make();
  }) as typeof fetch;
  return { fetch: fn, calls };
}

function memCache(seed?: CachedBundle): CacheStore & { store: Map<string, CachedBundle> } {
  const store = new Map<string, CachedBundle>();
  if (seed) store.set("testkind", seed);
  return {
    store,
    async read(kind: string): Promise<CachedBundle | null> {
      return store.get(kind) ?? null;
    },
    async write(kind: string, bundle: CachedBundle): Promise<void> {
      store.set(kind, bundle);
    },
  };
}

async function cachedBundleFor(payload: TestPayload, signer: TestSigner): Promise<CachedBundle> {
  const served = await signBundle(payload, signer);
  return {
    version: served.bundle.version,
    signed_at: served.bundle.signed_at,
    config: served.bundle.config,
    signature: served.bundle.signature,
  };
}

test("round-trips a signed bundle (source=remote)", async () => {
  const signer = await generateTestKeypair();
  const payload: TestPayload = { version: 2, value: "fresh" };
  const served = await signBundle(payload, signer);
  const bundleUrl = "/v1/config/testkind/bundle/2.json";
  const manifest = await manifestFor("testkind", served, bundleUrl);

  const { fetch } = mockFetch({
    [manifestUrl()]: () => new Response(JSON.stringify(manifest), { status: 200 }),
    [`${API}${bundleUrl}`]: () => new Response(served.bytes, { status: 200 }),
  });
  const env: RemoteConfigEnv = { apiUrl: API, publicKey: signer.publicKey, fetch };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "remote");
  assert.equal(result.version, 2);
  assert.equal(result.value.value, "fresh");
});

test("rejects a tampered signature → falls back to bundled", async () => {
  const signer = await generateTestKeypair();
  const served = await signBundle({ version: 2, value: "fresh" }, signer);
  // Flip a byte of the signature, then re-serve so the sha256 still
  // matches the (corrupted) body — isolating the signature failure.
  const badSig =
    (served.bundle.signature[0] === "A" ? "B" : "A") + served.bundle.signature.slice(1);
  const tampered = { bundle: { ...served.bundle, signature: badSig }, bytes: "" };
  tampered.bytes = JSON.stringify(tampered.bundle);
  const bundleUrl = "/v1/config/testkind/bundle/2.json";
  const manifest = await manifestFor("testkind", tampered, bundleUrl);

  const { fetch } = mockFetch({
    [manifestUrl()]: () => new Response(JSON.stringify(manifest), { status: 200 }),
    [`${API}${bundleUrl}`]: () => new Response(tampered.bytes, { status: 200 }),
  });
  const env: RemoteConfigEnv = { apiUrl: API, publicKey: signer.publicKey, fetch };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "bundled");
  assert.equal(result.value.value, "bundled");
});

test("rejects a sha256 mismatch → falls back to bundled", async () => {
  const signer = await generateTestKeypair();
  const served = await signBundle({ version: 2, value: "fresh" }, signer);
  const bundleUrl = "/v1/config/testkind/bundle/2.json";
  const manifest = await manifestFor("testkind", served, bundleUrl);
  const wrong = { ...manifest, sha256: "0".repeat(64) };

  const { fetch } = mockFetch({
    [manifestUrl()]: () => new Response(JSON.stringify(wrong), { status: 200 }),
    [`${API}${bundleUrl}`]: () => new Response(served.bytes, { status: 200 }),
  });
  const env: RemoteConfigEnv = { apiUrl: API, publicKey: signer.publicKey, fetch };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "bundled");
});

test("rejects a bundle signed by the wrong key → falls back to bundled", async () => {
  const signer = await generateTestKeypair();
  const attacker = await generateTestKeypair();
  const served = await signBundle({ version: 2, value: "evil" }, attacker);
  const bundleUrl = "/v1/config/testkind/bundle/2.json";
  const manifest = await manifestFor("testkind", served, bundleUrl);

  const { fetch } = mockFetch({
    [manifestUrl()]: () => new Response(JSON.stringify(manifest), { status: 200 }),
    [`${API}${bundleUrl}`]: () => new Response(served.bytes, { status: 200 }),
  });
  // Trust root is `signer`, but the bundle was signed by `attacker`.
  const env: RemoteConfigEnv = { apiUrl: API, publicKey: signer.publicKey, fetch };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "bundled");
  assert.equal(result.value.value, "bundled");
});

test("version gate: a not-newer manifest keeps the disk cache and skips the bundle GET", async () => {
  const signer = await generateTestKeypair();
  const cache = memCache(await cachedBundleFor({ version: 3, value: "cached-v3" }, signer));
  const served = await signBundle({ version: 2, value: "stale" }, signer);
  const bundleUrl = "/v1/config/testkind/bundle/2.json";
  const manifest = await manifestFor("testkind", served, bundleUrl);

  const { fetch, calls } = mockFetch({
    [manifestUrl()]: () => new Response(JSON.stringify(manifest), { status: 200 }),
    // If the loader fetches the bundle despite the gate, fail loudly.
    [`${API}${bundleUrl}`]: () => {
      throw new Error("bundle should not be fetched when version-gated");
    },
  });
  const env: RemoteConfigEnv = { apiUrl: API, publicKey: signer.publicKey, fetch, cache };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "disk");
  assert.equal(result.version, 3);
  assert.equal(result.value.value, "cached-v3");
  assert.ok(!calls.includes(`${API}${bundleUrl}`));
});

test("offline (manifest fails) → falls back to the last verified disk cache", async () => {
  const signer = await generateTestKeypair();
  const cache = memCache(await cachedBundleFor({ version: 3, value: "cached-v3" }, signer));
  const { fetch } = mockFetch({
    [manifestUrl()]: () => new Response("upstream down", { status: 503 }),
  });
  const env: RemoteConfigEnv = { apiUrl: API, publicKey: signer.publicKey, fetch, cache };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "disk");
  assert.equal(result.value.value, "cached-v3");
});

test("offline with no cache → falls back to the bundled default", async () => {
  const signer = await generateTestKeypair();
  const { fetch } = mockFetch({
    [manifestUrl()]: () => {
      throw new Error("network down");
    },
  });
  const env: RemoteConfigEnv = { apiUrl: API, publicKey: signer.publicKey, fetch };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "bundled");
  assert.equal(result.value.value, "bundled");
});

test("a tampered disk cache is rejected, not served", async () => {
  const signer = await generateTestKeypair();
  const good = await cachedBundleFor({ version: 3, value: "cached-v3" }, signer);
  // Corrupt the signed bytes so they no longer match the signature.
  const tampered: CachedBundle = { ...good, config: bumpFirstChar(good.config) };
  const cache = memCache(tampered);
  const { fetch } = mockFetch({
    [manifestUrl()]: () => {
      throw new Error("offline");
    },
  });
  const env: RemoteConfigEnv = { apiUrl: API, publicKey: signer.publicKey, fetch, cache };

  const result = await loadRemoteConfig(testKind(), env);
  // Tampered cache ignored → bundled, never the attacker's bytes.
  assert.equal(result.source, "bundled");
  assert.equal(result.value.value, "bundled");
});

test("RemoteConfigStore: bundled → disk (init) → remote (refresh), and a failed refresh never downgrades", async () => {
  const signer = await generateTestKeypair();
  const cache = memCache(await cachedBundleFor({ version: 3, value: "cached-v3" }, signer));
  const served = await signBundle({ version: 5, value: "remote-v5" }, signer);
  const bundleUrl = "/v1/config/testkind/bundle/5.json";
  const manifest = await manifestFor("testkind", served, bundleUrl);

  let online = true;
  const fetchImpl = (async (input: string | URL | Request): Promise<Response> => {
    if (!online) throw new Error("offline");
    const url = typeof input === "string" ? input : input.toString();
    if (url === manifestUrl()) return new Response(JSON.stringify(manifest), { status: 200 });
    if (url === `${API}${bundleUrl}`) return new Response(served.bytes, { status: 200 });
    return new Response("404", { status: 404 });
  }) as typeof globalThis.fetch;
  const env: RemoteConfigEnv = {
    apiUrl: API,
    publicKey: signer.publicKey,
    fetch: fetchImpl,
    cache,
  };

  const store = new RemoteConfigStore(env, [testKind()]);
  // Before anything: bundled.
  assert.equal(store.getResult<TestPayload>("testkind").source, "bundled");
  assert.equal(store.get<TestPayload>("testkind").value, "bundled");

  // Disk-first (no network): the v3 cache.
  await store.initFromCache();
  assert.equal(store.getResult<TestPayload>("testkind").source, "disk");
  assert.equal(store.get<TestPayload>("testkind").value, "cached-v3");

  // Refresh: the v5 remote bundle, written through to the cache.
  await store.refreshAll();
  assert.equal(store.getResult<TestPayload>("testkind").source, "remote");
  assert.equal(store.get<TestPayload>("testkind").value, "remote-v5");
  assert.equal(cache.store.get("testkind")?.version, 5);

  // Network drops: a failed refresh keeps the last good value.
  online = false;
  await store.refreshAll();
  assert.equal(store.get<TestPayload>("testkind").value, "remote-v5");
});

function bumpFirstChar(b64: string): string {
  const first = b64[0] === "A" ? "B" : "A";
  return first + b64.slice(1);
}
