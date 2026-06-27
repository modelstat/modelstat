/**
 * Coalescing single-flight runner.
 *
 * Wraps an async task so that **at most one invocation runs at a time**
 * and overlapping requests collapse into a single follow-up run rather
 * than stacking up.
 *
 * Why this exists — it is the load-bearing bound on the daemon's memory.
 * The daemon (apps/daemon/src/daemon.ts) triggers `runScanCycle` from
 * three places: the startup boot, the chokidar file-watcher (debounced),
 * and a 5-minute backstop `setInterval`. None of those checked whether a
 * scan was already in progress. During a long backfill a single
 * `scanAll()` runs for *hours* — the per-segment Qwen summariser is the
 * hard throughput bottleneck and every scan funnels through one
 * serialized inference queue — so the backstop and watcher kept admitting
 * fresh, concurrent `scanAll()` runs faster than they could finish. Each
 * parked scan retains its history-sized `jobs` array plus its event
 * buffers for its entire (now very long) lifetime, so in-flight scan
 * state accumulated without limit until V8's old-space hit
 * `--max-old-space-size` and the process died with
 *   "FATAL ERROR: Ineffective mark-compacts near heap limit".
 *
 * Routing every trigger through one coalescing runner caps concurrent
 * scan state to exactly one `scanAll()`, which holds steady memory across
 * an unbounded backfill. A request that arrives mid-scan is not dropped:
 * exactly one more scan runs after the current one drains, so the final
 * scan always reflects the most recent on-disk state.
 *
 * Guarantees:
 *   - `task` never runs concurrently with itself.
 *   - If `trigger()` is called while `task` is running, exactly one more
 *     run happens after the current one completes. Multiple triggers
 *     during a run coalesce into that single follow-up; the most recent
 *     argument wins.
 *   - `task` errors are swallowed (the task owns its own error
 *     reporting): a throw never drops a queued follow-up and never
 *     escapes as an unhandled rejection from a `void trigger(...)` call.
 */
export interface CoalescingRunner<A> {
  /**
   * Request a run. Starts immediately if idle; otherwise records that
   * one more run is needed once the active run finishes. Returns the
   * promise for the currently-running chain (settles when the runner
   * goes idle), so a caller that wants to await completion — e.g. the
   * startup boot — can.
   */
  trigger: (arg: A) => Promise<void>;
  /** True while a run is in progress. Exposed for tests / diagnostics. */
  isRunning: () => boolean;
  /** True while a follow-up run is queued behind the active one. */
  isPending: () => boolean;
  /**
   * Resolve once the runner is idle — the active chain (plus any coalesced
   * follow-up it picks up) has fully drained. Resolves immediately when idle.
   * Used at shutdown to let an in-flight scan settle before tearing down the
   * bundled summariser's Metal device (a scan mid-inference + a device free =
   * the llama.cpp teardown abort). Never rejects — the chain swallows errors.
   */
  idle: () => Promise<void>;
}

export function createCoalescingRunner<A>(
  task: (arg: A) => Promise<void>,
): CoalescingRunner<A> {
  let running = false;
  // The single coalesced follow-up request, if any. Only the most
  // recent one is kept — that is the whole point of coalescing.
  let next: { arg: A } | null = null;
  let chain: Promise<void> = Promise.resolve();

  function trigger(arg: A): Promise<void> {
    if (running) {
      // A scan is active — do NOT start a concurrent one. Remember that
      // another pass is needed and which reason triggered it; the
      // running chain will pick it up when it next checks.
      next = { arg };
      return chain;
    }
    running = true;
    chain = (async () => {
      let cur: A = arg;
      try {
        for (;;) {
          try {
            await task(cur);
          } catch {
            // task reports its own failures; swallow so a throw can't
            // drop the coalesced follow-up below or orphan a rejection.
          }
          // No `await` between this check and clearing `running` in the
          // finally block, so a trigger() cannot race into that gap:
          // either it arrived during `await task(...)` (caught here) or
          // it will observe `running === false` and start a fresh chain.
          if (!next) break;
          cur = next.arg;
          next = null;
        }
      } finally {
        running = false;
      }
    })();
    return chain;
  }

  // Await the current chain to its end. `chain` is reassigned per fresh run, so
  // awaiting the latest reference settles after the active run AND any follow-up
  // it coalesces. `chain` never rejects (the loop swallows task errors), so this
  // resolves cleanly even on a failing scan.
  async function idle(): Promise<void> {
    await chain;
  }

  return {
    trigger,
    isRunning: () => running,
    isPending: () => next !== null,
    idle,
  };
}
