/**
 * Defaults and sizing constants shared across companions.
 *
 * One source of truth so CLI and extension stop drifting. The
 * companion-unification doc spells out the rationale for each choice.
 */

/** Upload cadence: every 5 s. */
export const INGEST_BATCH_INTERVAL_MS = 5_000 as const;

/** Max events per batch. 1000 splits the old CLI (2000) and extension
 * (500) figures — gives both sides an identical rate-limit profile. */
export const INGEST_BATCH_MAX_EVENTS = 1_000 as const;

/** Hold back events until the session has been quiet this long. Keeps
 * related turns together, gives the summariser meaningful windows. */
export const SESSION_DEBOUNCE_MS = 7_000 as const;

/** Even if the session isn't quiet yet, force a ship if this many
 * unsent events have accumulated on it. */
export const FORCE_SHIP_THRESHOLD = 200 as const;

/** Retry backoff schedule for 429/5xx. Capped at 60 s. */
export const BACKOFF_MS = [1_000, 2_500, 5_000, 10_000, 20_000, 60_000] as const;
export function expBackoff(attempt: number): number {
  const i = Math.min(Math.max(attempt, 0), BACKOFF_MS.length - 1);
  // TypeScript can't prove `i` is in range with noUncheckedIndexedAccess;
  // we clamped above so the non-null assertion is safe.
  // biome-ignore lint/style/noNonNullAssertion: clamped above
  return BACKOFF_MS[i]!;
}

/** How long the CLI waits between filesystem-watcher backstop scans. */
export const BACKSTOP_SCAN_MS = 5 * 60_000;

/** Heartbeat intervals differ by runtime (progress granularity differs).
 * Keep them visible in one place so drift is deliberate. */
export const HEARTBEAT_INTERVAL_MS_CLI = 10_000 as const;
export const HEARTBEAT_INTERVAL_MS_EXTENSION = 60_000 as const;
