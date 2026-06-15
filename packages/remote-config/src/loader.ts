/**
 * The unified loader. One pipeline, one fallback ladder, reused by every
 * config kind on both Node and the browser.
 *
 *   loadRemoteConfig(kind, env):
 *     1. read the disk cache (re-validated)               ── cheap, offline-safe
 *     2. GET {api}/v1/config/{kind}                        ── the current payload
 *     3. validate the payload + version-gate vs the cache  ── ignore a stale/older one
 *     4. write-through to the cache, return the new value
 *
 *   FALLBACK LADDER (any failure — offline / schema-invalid / older-version):
 *   remote → disk (last good) → bundled default. Fail-closed: a failure
 *   never clobbers good state.
 *
 * Trust is the TLS connection to the modelstat origin; the client only
 * validates the payload's shape and version. `RemoteConfigStore` adds the
 * long-lived-daemon shape: seed from disk at startup (instant, no network),
 * then refresh on a timer, always serving the best value held in memory.
 */

import type { ConfigKind, ConfigSource, LoadResult, RemoteConfigEnv } from "./types.js";

type AnyConfigKind = ConfigKind<{ version: number }>;

function getFetch(env: RemoteConfigEnv): typeof fetch {
  return env.fetch ?? fetch;
}

/** GET + validate the current payload for a kind. Null on any failure. */
async function fetchConfig<T extends { version: number }>(
  kind: ConfigKind<T>,
  env: RemoteConfigEnv,
): Promise<{ value: T; version: number } | null> {
  const url = `${env.apiUrl.replace(/\/+$/, "")}/v1/config/${encodeURIComponent(kind.kind)}`;
  let body: unknown;
  try {
    const res = await getFetch(env)(url);
    if (!res.ok) throw new Error(`status ${res.status}`);
    body = await res.json();
  } catch (e) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} fetch failed`, e);
    return null;
  }
  const parsed = kind.schema.safeParse(body);
  if (!parsed.success) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} payload schema invalid`);
    return null;
  }
  return { value: parsed.data, version: parsed.data.version };
}

/**
 * Read the cache and RE-VALIDATE it against the kind schema. We never trust
 * bytes on disk just because we wrote them once — a torn or tampered file
 * is rejected exactly like a bad network response, and we fall through.
 */
async function readCachedConfig<T extends { version: number }>(
  kind: ConfigKind<T>,
  env: RemoteConfigEnv,
): Promise<{ value: T; version: number } | null> {
  if (!env.cache) return null;
  let raw: unknown;
  try {
    raw = await env.cache.read(kind.kind);
  } catch (e) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} cache read failed`, e);
    return null;
  }
  if (raw == null) return null;
  const parsed = kind.schema.safeParse(raw);
  if (!parsed.success) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} disk cache rejected`);
    return null;
  }
  return { value: parsed.data, version: parsed.data.version };
}

async function writeCache(
  kind: AnyConfigKind,
  env: RemoteConfigEnv,
  value: unknown,
): Promise<void> {
  if (!env.cache) return;
  try {
    await env.cache.write(kind.kind, value);
  } catch (e) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} cache write failed`, e);
  }
}

/**
 * One-shot load with the full ladder: try remote (version-gated against the
 * disk cache), else the last good disk cache, else the bundled default.
 * Always resolves to a usable value.
 */
export async function loadRemoteConfig<T extends { version: number }>(
  kind: ConfigKind<T>,
  env: RemoteConfigEnv,
): Promise<LoadResult<T>> {
  const cached = await readCachedConfig(kind, env);
  const fetched = await fetchConfig(kind, env);

  if (fetched && (!cached || fetched.version >= cached.version)) {
    // Only touch the disk when the value actually moved forward.
    if (!cached || fetched.version > cached.version) await writeCache(kind, env, fetched.value);
    return result(kind.kind, fetched.value, fetched.version, "remote");
  }
  if (cached) return result(kind.kind, cached.value, cached.version, "disk");
  return result(kind.kind, kind.bundledFallback, kind.bundledFallback.version, "bundled");
}

function result<T>(kind: string, value: T, version: number, source: ConfigSource): LoadResult<T> {
  return { kind, value, version, source };
}

interface StateEntry {
  value: { version: number };
  version: number;
  source: ConfigSource;
}

/**
 * Long-lived in-memory registry over many kinds. Built for the daemon:
 * seed from disk at startup, expose an instant `get`, and `refresh` on a
 * timer. A failed refresh keeps the current value.
 */
export class RemoteConfigStore {
  private readonly env: RemoteConfigEnv;
  private readonly kinds: Map<string, AnyConfigKind>;
  private readonly state = new Map<string, StateEntry>();

  constructor(env: RemoteConfigEnv, kinds: readonly AnyConfigKind[]) {
    this.env = env;
    this.kinds = new Map(kinds.map((k) => [k.kind, k]));
    // Seed every kind with its bundled fallback so `get` is total from the
    // very first tick, before any disk read or network call.
    for (const k of kinds) {
      this.state.set(k.kind, {
        value: k.bundledFallback,
        version: k.bundledFallback.version,
        source: "bundled",
      });
    }
  }

  /** Seed from the disk cache without touching the network — the instant,
   * offline-safe startup path. Call once, then `refreshAll` in the
   * background. Anything not on disk (or rejected) stays on its fallback. */
  async initFromCache(): Promise<void> {
    await Promise.all(
      [...this.kinds.values()].map(async (k) => {
        const cached = await readCachedConfig(k, this.env);
        const current = this.state.get(k.kind);
        if (cached && (!current || cached.version >= current.version)) {
          this.state.set(k.kind, { value: cached.value, version: cached.version, source: "disk" });
        }
      }),
    );
  }

  /** Fetch one kind, swapping it in only if strictly newer. Never throws
   * and never downgrades the held value on failure. */
  async refresh(kindName: string): Promise<void> {
    const kind = this.kinds.get(kindName);
    if (!kind) throw new Error(`unknown config kind: ${kindName}`);
    const current = this.state.get(kindName);
    const currentVersion = current?.version ?? kind.bundledFallback.version;

    const fetched = await fetchConfig(kind, this.env);
    if (!fetched) return;
    if (fetched.version <= currentVersion) return; // version gate

    await writeCache(kind, this.env, fetched.value);
    this.state.set(kindName, { value: fetched.value, version: fetched.version, source: "remote" });
  }

  async refreshAll(): Promise<void> {
    await Promise.all([...this.kinds.keys()].map((k) => this.refresh(k)));
  }

  /** The current best value for a kind. Total — falls back to bundled. */
  get<T extends { version: number }>(kindName: string): T {
    const entry = this.state.get(kindName);
    if (!entry) throw new Error(`unknown config kind: ${kindName}`);
    return entry.value as T;
  }

  /** Like `get`, but also reports the version + provenance. */
  getResult<T extends { version: number }>(kindName: string): LoadResult<T> {
    const entry = this.state.get(kindName);
    if (!entry) throw new Error(`unknown config kind: ${kindName}`);
    return result(kindName, entry.value as T, entry.version, entry.source);
  }
}
