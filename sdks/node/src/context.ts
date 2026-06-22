/**
 * The ambient metadata context layer.
 *
 * `withMetadata({...}, fn)` runs `fn` with extra attribution tags in scope; any
 * `record()` that happens during `fn` (synchronously or across `await`s)
 * resolves those tags as the **middle** tier — above `Config` defaults, below
 * per-call tags. The scope auto-resets when `fn` returns or throws, because the
 * tags live in an {@link AsyncLocalStorage} store that only covers `fn`'s async
 * execution.
 *
 * Nested scopes merge (an inner `withMetadata` sees the outer tags too), with
 * the inner value winning on a shared key.
 */

import { AsyncLocalStorage } from "node:async_hooks";
import type { Metadata } from "./wire.js";

/** Holds the ambient tag map for the current async execution context. */
const storage = new AsyncLocalStorage<Metadata>();

/**
 * Run `fn` with `tags` contributed to the ambient metadata layer for the
 * duration of its (async) execution, then restore the previous ambient tags.
 *
 * Returns whatever `fn` returns (its value or promise). Tags from an enclosing
 * `withMetadata` are inherited and merged, with `tags` winning on shared keys.
 */
export function withMetadata<T>(tags: Metadata, fn: () => T): T {
  const parent = storage.getStore();
  const merged: Metadata = parent ? { ...parent, ...tags } : { ...tags };
  return storage.run(merged, fn);
}

/**
 * The ambient tag map currently in scope (a shallow copy so callers can't
 * mutate the live store), or `undefined` when no `withMetadata` is active.
 */
export function ambientMetadata(): Metadata | undefined {
  const store = storage.getStore();
  return store ? { ...store } : undefined;
}
