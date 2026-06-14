/**
 * Helpers for loading OPTIONAL runtime dependencies via dynamic
 * `import()` without ever putting a *failing* import on a hot path.
 *
 * Why this exists: a rejected dynamic `import()` is not free. Node has
 * to re-run ESM resolution (a chain of filesystem probes) on every
 * attempt, allocates a fresh `ERR_MODULE_NOT_FOUND` error + stack, and
 * the V8 loader retains per-attempt bookkeeping — repeated failing
 * imports grow the heap without bound. The 2026-06-11 daemon OOM was
 * exactly this: `@huggingface/transformers` missing from the staged
 * runtime, the embedder retrying the import once per *turn* during the
 * v1→v3 full-corpus reprocess — ~5 million failed imports, ~1 GB of
 * identical warn lines in err.log, and an 8 GB V8 heap at crash.
 *
 * The rule encoded here: an optional-dep loader may retry a TRANSIENT
 * failure a small bounded number of times, but a missing package is
 * PERMANENT for the life of the process — latch it, warn once, and
 * answer "unavailable" from then on without touching the loader again.
 */

/** Max load attempts for failures that don't look permanent (e.g. a
 * flaky network during a model download). After this many consecutive
 * failures the loader must latch unavailable for the process lifetime. */
export const OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS = 3;

/**
 * True when the error says the module simply isn't installed/resolvable
 * — there is no point ever retrying the import in this process.
 */
export function isMissingOptionalModuleError(err: unknown): boolean {
  const code = (err as { code?: string } | null)?.code;
  if (code === "ERR_MODULE_NOT_FOUND" || code === "MODULE_NOT_FOUND") return true;
  const msg = err instanceof Error ? err.message : String(err);
  return /cannot find (package|module)|cannot resolve|failed to resolve/i.test(msg);
}
