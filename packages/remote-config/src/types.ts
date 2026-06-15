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
 * payload must carry a numeric `version`.
 */
export interface ConfigKind<T extends { version: number }> {
  kind: string;
  schema: ZodType<T>;
  bundledFallback: T;
}

/**
 * Pluggable persistence for the last good config payload, per kind. The
 * daemon backs this with disk (`./node.ts` — the piece the extension's
 * in-memory-only model lacks); a browser could back it with IndexedDB;
 * tests use the in-memory store. The stored value is the raw payload — the
 * loader re-validates it on read, so a torn or tampered file is simply
 * rejected and treated as "no cache".
 */
export interface CacheStore {
  read(kind: string): Promise<unknown | null>;
  write(kind: string, payload: unknown): Promise<void>;
}

/** Everything the loader needs from its host environment. */
export interface RemoteConfigEnv {
  /** Base URL for the config API, e.g. `https://modelstat.ai`. */
  apiUrl: string;
  /** Injectable fetch; defaults to the global `fetch`. */
  fetch?: typeof fetch;
  /** Persistence for the last good payload; omit for memory-only operation. */
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
