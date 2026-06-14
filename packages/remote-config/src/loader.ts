/**
 * The unified loader. One pipeline, one fallback ladder, reused by every
 * config kind on both Node and the browser.
 *
 *   loadRemoteConfig(kind, env):
 *     1. read disk cache (re-verified)                       ── cheap, offline-safe
 *     2. GET {api}/v1/config/{kind}/manifest.json            ── cheap pointer
 *     3. if manifest.version <= cached.version → keep cache   ── version gate
 *     4. GET bundle_url → sha256 check → verify Ed25519 over
 *        RAW bytes → JSON.parse → schema → cross-check version
 *     5. write-through to the cache, return the new value
 *
 *   FALLBACK LADDER (any failure — offline / bad-sig / schema-invalid /
 *   sha-mismatch / unknown-version): remote → disk (last verified) →
 *   bundled default. Fail-closed: a failure never clobbers good state.
 *
 * `RemoteConfigStore` adds the long-lived-daemon shape on top: seed from
 * disk at startup (instant, no network), then refresh on a timer, always
 * serving the best value held in memory.
 */

import { base64ToBytes, sha256Hex } from "./crypto.js";
import { ConfigManifest } from "./schema.js";
import type {
  CachedBundle,
  ConfigKind,
  ConfigSource,
  LoadResult,
  RemoteConfigEnv,
} from "./types.js";
import { verifyConfigBytes, verifySignedBundle } from "./verify.js";

type AnyConfigKind = ConfigKind<{ version: number }>;

function getFetch(env: RemoteConfigEnv): typeof fetch {
  return env.fetch ?? fetch;
}

function joinUrl(base: string, path: string): string {
  if (path.startsWith("http://") || path.startsWith("https://")) return path;
  return `${base.replace(/\/+$/, "")}/${path.replace(/^\/+/, "")}`;
}

/** GET + validate the cheap manifest pointer. Returns null on any failure. */
async function fetchManifest(
  kind: AnyConfigKind,
  env: RemoteConfigEnv,
): Promise<ConfigManifest | null> {
  const url = `${env.apiUrl.replace(/\/+$/, "")}/v1/config/${encodeURIComponent(kind.kind)}/manifest.json`;
  try {
    const res = await getFetch(env)(url);
    if (!res.ok) throw new Error(`status ${res.status}`);
    const parsed = ConfigManifest.safeParse(await res.json());
    if (!parsed.success) {
      env.logger?.warn?.(`[remote-config] ${kind.kind} manifest schema invalid`);
      return null;
    }
    if (parsed.data.kind !== kind.kind) {
      env.logger?.warn?.(`[remote-config] ${kind.kind} manifest kind mismatch`);
      return null;
    }
    return parsed.data;
  } catch (e) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} manifest fetch failed`, e);
    return null;
  }
}

/**
 * GET the signed bundle named by a manifest, check its sha256, verify the
 * signature, and validate the payload. Returns the verified value plus
 * the envelope to persist, or null on any failure.
 */
async function fetchVerifiedBundle<T extends { version: number }>(
  kind: ConfigKind<T>,
  manifest: ConfigManifest,
  env: RemoteConfigEnv,
): Promise<{ value: T; version: number; cached: CachedBundle } | null> {
  const url = joinUrl(env.apiUrl, manifest.bundle_url);
  let bytes: Uint8Array;
  let envelope: unknown;
  try {
    const res = await getFetch(env)(url);
    if (!res.ok) throw new Error(`status ${res.status}`);
    bytes = new Uint8Array(await res.arrayBuffer());
  } catch (e) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} bundle fetch failed`, e);
    return null;
  }

  // Transport-integrity cross-check against the manifest (the signature
  // is still the trust anchor; this just catches a truncated/garbled body).
  const digest = await sha256Hex(bytes);
  if (digest !== manifest.sha256) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} bundle sha256 mismatch`);
    return null;
  }

  try {
    envelope = JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    env.logger?.warn?.(`[remote-config] ${kind.kind} bundle not json`);
    return null;
  }

  const verified = await verifySignedBundle({
    envelope,
    publicKey: env.publicKey,
    schema: kind.schema,
  });
  if (!verified.ok) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} bundle rejected: ${verified.reason}`);
    return null;
  }
  if (verified.version !== manifest.version) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} bundle/manifest version disagree`);
    return null;
  }

  const env2 = envelope as { config: string; signature: string; signed_at: string };
  return {
    value: verified.value,
    version: verified.version,
    cached: {
      version: verified.version,
      signed_at: env2.signed_at,
      config: env2.config,
      signature: env2.signature,
    },
  };
}

/**
 * Read the cache and RE-VERIFY it. We never trust bytes on disk just
 * because we wrote them once — a cache an attacker swapped under us is
 * rejected exactly like a bad network response.
 */
async function readVerifiedCache<T extends { version: number }>(
  kind: ConfigKind<T>,
  env: RemoteConfigEnv,
): Promise<{ value: T; version: number } | null> {
  if (!env.cache) return null;
  let cached: CachedBundle | null;
  try {
    cached = await env.cache.read(kind.kind);
  } catch (e) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} cache read failed`, e);
    return null;
  }
  if (!cached) return null;

  const verified = await verifyConfigBytes({
    configBytes: base64ToBytes(cached.config),
    signature: base64ToBytes(cached.signature),
    publicKey: env.publicKey,
    schema: kind.schema,
    expectedVersion: cached.version,
  });
  if (!verified.ok) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} disk cache rejected: ${verified.reason}`);
    return null;
  }
  return { value: verified.value, version: verified.version };
}

async function writeCache(
  kind: AnyConfigKind,
  env: RemoteConfigEnv,
  cached: CachedBundle,
): Promise<void> {
  if (!env.cache) return;
  try {
    await env.cache.write(kind.kind, cached);
  } catch (e) {
    env.logger?.warn?.(`[remote-config] ${kind.kind} cache write failed`, e);
  }
}

/**
 * One-shot load with the full ladder: try remote (version-gated against
 * the disk cache), else the last verified disk cache, else the bundled
 * default. Always resolves to a usable value.
 */
export async function loadRemoteConfig<T extends { version: number }>(
  kind: ConfigKind<T>,
  env: RemoteConfigEnv,
): Promise<LoadResult<T>> {
  const cached = await readVerifiedCache(kind, env);
  const manifest = await fetchManifest(kind, env);

  if (manifest) {
    // Version gate: a manifest no newer than the cache needs no bundle GET.
    if (!cached || manifest.version > cached.version) {
      const fetched = await fetchVerifiedBundle(kind, manifest, env);
      if (fetched) {
        await writeCache(kind, env, fetched.cached);
        return result(kind.kind, fetched.value, fetched.version, "remote");
      }
    }
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
 * seed from disk at construction-time intent, expose an instant `get`,
 * and `refresh` on a timer. A failed refresh keeps the current value.
 */
export class RemoteConfigStore {
  private readonly env: RemoteConfigEnv;
  private readonly kinds: Map<string, AnyConfigKind>;
  private readonly state = new Map<string, StateEntry>();

  constructor(env: RemoteConfigEnv, kinds: readonly AnyConfigKind[]) {
    this.env = env;
    this.kinds = new Map(kinds.map((k) => [k.kind, k]));
    // Seed every kind with its bundled fallback so `get` is total from
    // the very first tick, before any disk read or network call.
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
        const cached = await readVerifiedCache(k, this.env);
        const current = this.state.get(k.kind);
        if (cached && (!current || cached.version >= current.version)) {
          this.state.set(k.kind, { value: cached.value, version: cached.version, source: "disk" });
        }
      }),
    );
  }

  /** Fetch + verify one kind, swapping it in only if strictly newer.
   * Never throws and never downgrades the held value on failure. */
  async refresh(kindName: string): Promise<void> {
    const kind = this.kinds.get(kindName);
    if (!kind) throw new Error(`unknown config kind: ${kindName}`);
    const current = this.state.get(kindName);
    const currentVersion = current?.version ?? kind.bundledFallback.version;

    const manifest = await fetchManifest(kind, this.env);
    if (!manifest) return;
    if (manifest.version <= currentVersion) return; // version gate

    const fetched = await fetchVerifiedBundle(kind, manifest, this.env);
    if (!fetched) return;

    await writeCache(kind, this.env, fetched.cached);
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
