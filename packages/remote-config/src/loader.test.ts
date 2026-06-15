/**
 * Loader + fallback-ladder tests. Everything runs against a routed mock
 * `fetch` (no network), proving: a config payload round-trips; a
 * schema-invalid payload is rejected; the version gate holds; offline →
 * disk → bundled is proven; and a failure never clobbers good state.
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import { z } from "zod";
import { loadRemoteConfig, RemoteConfigStore } from "./loader.js";
import type { CacheStore, ConfigKind, RemoteConfigEnv } from "./types.js";

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

function configUrl(): string {
  return `${API}/v1/config/testkind`;
}

/** A routed mock fetch. Each route is a thunk so a test can assert a route
 * is never hit (by throwing) or count calls. */
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

function memCache(seed?: unknown): CacheStore & { store: Map<string, unknown> } {
  const store = new Map<string, unknown>();
  if (seed !== undefined) store.set("testkind", seed);
  return {
    store,
    async read(kind: string): Promise<unknown | null> {
      return store.has(kind) ? (store.get(kind) ?? null) : null;
    },
    async write(kind: string, payload: unknown): Promise<void> {
      store.set(kind, payload);
    },
  };
}

function ok(payload: TestPayload): () => Response {
  return () => new Response(JSON.stringify(payload), { status: 200 });
}

test("round-trips a config payload (source=remote)", async () => {
  const { fetch } = mockFetch({ [configUrl()]: ok({ version: 2, value: "fresh" }) });
  const env: RemoteConfigEnv = { apiUrl: API, fetch };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "remote");
  assert.equal(result.version, 2);
  assert.equal(result.value.value, "fresh");
});

test("rejects a schema-invalid payload → falls back to bundled", async () => {
  const { fetch } = mockFetch({
    // value must be a string; a number is rejected by the kind schema.
    [configUrl()]: () => new Response(JSON.stringify({ version: 2, value: 123 }), { status: 200 }),
  });
  const env: RemoteConfigEnv = { apiUrl: API, fetch };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "bundled");
  assert.equal(result.value.value, "bundled");
});

test("version gate: a not-newer payload keeps the disk cache", async () => {
  const cache = memCache({ version: 3, value: "cached-v3" });
  const { fetch } = mockFetch({ [configUrl()]: ok({ version: 2, value: "stale" }) });
  const env: RemoteConfigEnv = { apiUrl: API, fetch, cache };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "disk");
  assert.equal(result.version, 3);
  assert.equal(result.value.value, "cached-v3");
});

test("offline (fetch fails) → falls back to the last good disk cache", async () => {
  const cache = memCache({ version: 3, value: "cached-v3" });
  const { fetch } = mockFetch({
    [configUrl()]: () => new Response("upstream down", { status: 503 }),
  });
  const env: RemoteConfigEnv = { apiUrl: API, fetch, cache };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "disk");
  assert.equal(result.value.value, "cached-v3");
});

test("offline with no cache → falls back to the bundled default", async () => {
  const { fetch } = mockFetch({
    [configUrl()]: () => {
      throw new Error("network down");
    },
  });
  const env: RemoteConfigEnv = { apiUrl: API, fetch };

  const result = await loadRemoteConfig(testKind(), env);
  assert.equal(result.source, "bundled");
  assert.equal(result.value.value, "bundled");
});

test("a corrupt disk cache is rejected, not served", async () => {
  // A cached blob that no longer matches the schema (value must be a string).
  const cache = memCache({ version: 3, value: { nested: "junk" } });
  const { fetch } = mockFetch({
    [configUrl()]: () => {
      throw new Error("offline");
    },
  });
  const env: RemoteConfigEnv = { apiUrl: API, fetch, cache };

  const result = await loadRemoteConfig(testKind(), env);
  // Corrupt cache ignored → bundled, never the junk bytes.
  assert.equal(result.source, "bundled");
  assert.equal(result.value.value, "bundled");
});

test("RemoteConfigStore: bundled → disk (init) → remote (refresh), and a failed refresh never downgrades", async () => {
  const cache = memCache({ version: 3, value: "cached-v3" });
  let online = true;
  const fetchImpl = (async (input: string | URL | Request): Promise<Response> => {
    if (!online) throw new Error("offline");
    const url = typeof input === "string" ? input : input.toString();
    if (url === configUrl()) {
      return new Response(JSON.stringify({ version: 5, value: "remote-v5" }), { status: 200 });
    }
    return new Response("404", { status: 404 });
  }) as typeof globalThis.fetch;
  const env: RemoteConfigEnv = { apiUrl: API, fetch: fetchImpl, cache };

  const store = new RemoteConfigStore(env, [testKind()]);
  // Before anything: bundled.
  assert.equal(store.getResult<TestPayload>("testkind").source, "bundled");
  assert.equal(store.get<TestPayload>("testkind").value, "bundled");

  // Disk-first (no network): the v3 cache.
  await store.initFromCache();
  assert.equal(store.getResult<TestPayload>("testkind").source, "disk");
  assert.equal(store.get<TestPayload>("testkind").value, "cached-v3");

  // Refresh: the v5 remote payload, written through to the cache.
  await store.refreshAll();
  assert.equal(store.getResult<TestPayload>("testkind").source, "remote");
  assert.equal(store.get<TestPayload>("testkind").value, "remote-v5");
  assert.equal((cache.store.get("testkind") as TestPayload | undefined)?.version, 5);

  // Network drops: a failed refresh keeps the last good value.
  online = false;
  await store.refreshAll();
  assert.equal(store.get<TestPayload>("testkind").value, "remote-v5");
});
