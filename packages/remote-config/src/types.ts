/**
 * Shared interfaces for the loader. The host environment (Node daemon,
 * extension service worker, or a test) supplies a `RemoteConfigEnv`; the
 * loader stays free of any platform-specific code.
 */

import type { ZodType } from "zod";

/** A minimal logger sink. Every method is optional and the package
 * defaults to silence, so a host opts in to whatever noise it wants. */
export interface Logger {
  info?(msg: string, ...args: unknown[]): void;
  warn?(msg: string, ...args: unknown[]): void;
  error?(msg: string, ...args: unknown[]): void;
}

/**
 * One config kind: its name, its payload schema, and the value compiled
 * into the binary that is used when nothing better is available. The
 * payload must be `VersionedConfig` (carry a numeric `version`).
 */
export interface ConfigKind<T extends { version: number }> {
  kind: string;
  schema: ZodType<T>;
  bundledFallback: T;
}

/** A verified bundle as persisted to — and re-verified from — the cache. */
export interface CachedBundle {
  version: number;
  signed_at: string;
  /** Base64 of the signed config bytes. */
  config: string;
  /** Base64 Ed25519 signature over the config bytes. */
  signature: string;
}

/**
 * Pluggable persistence for the last verified bundle, per kind. The
 * daemon backs this with disk (`./node.ts` — the piece the extension's
 * in-memory-only model lacks); a browser could back it with IndexedDB;
 * tests use the in-memory store.
 */
export interface CacheStore {
  read(kind: string): Promise<CachedBundle | null>;
  write(kind: string, bundle: CachedBundle): Promise<void>;
}

/** Everything the loader needs from its host environment. */
export interface RemoteConfigEnv {
  /** Base URL for the config API, e.g. `https://modelstat.ai`. */
  apiUrl: string;
  /** Raw 32-byte Ed25519 public key — the bundled trust root. */
  publicKey: Uint8Array;
  /** Injectable fetch; defaults to the global `fetch`. */
  fetch?: typeof fetch;
  /** Persistence for verified bundles; omit for memory-only operation. */
  cache?: CacheStore;
  logger?: Logger;
}

/** Where the currently-held value came from, best → worst. */
export type ConfigSource = "remote" | "disk" | "bundled";

export interface LoadResult<T> {
  kind: string;
  value: T;
  version: number;
  source: ConfigSource;
}
