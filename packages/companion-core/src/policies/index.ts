/**
 * Wire the `policies` augment into the long-lived daemon. Loads the
 * `policies` config kind via `@modelstat/remote-config` (disk-cache-first,
 * additive-only) and applies the patterns to the in-process redaction floor.
 *
 * The floor itself is irreducible and compiled into the binary
 * (`@modelstat/core/redact`); this path can only ever ADD patterns over it. If
 * the network, the disk cache, and the bundled fallback all somehow produced
 * nothing, the floor still applies — fail-closed by construction. A malformed
 * payload, a stale bundle, or an offline boot all degrade to "floor only",
 * never to "floor weakened".
 */

import {
  compilePolicyPatterns,
  POLICIES_CONFIG_KIND,
  type RedactionPolicyBundle,
  setRemoteRedactionPatterns,
} from "@modelstat/core";
import {
  type ConfigKind,
  type Logger,
  type RemoteConfigEnv,
  RemoteConfigStore,
} from "@modelstat/remote-config";
import { createNodeDiskCache } from "@modelstat/remote-config/node";

const DEFAULT_REFRESH_MS = 15 * 60 * 1000;

export interface PolicyRefresherOptions {
  /** API origin, e.g. `https://modelstat.ai`. */
  apiUrl: string;
  /** Injectable fetch (tests). Defaults to the global `fetch`. */
  fetch?: typeof fetch;
  /** Override the disk-cache dir (tests). Default `~/.modelstat/config`. */
  cacheDir?: string;
  /** Refresh cadence; default 15 min (matches the daemon's other timers). */
  intervalMs?: number;
  logger?: Logger;
}

export interface PolicyRefresher {
  /** Seed from disk (offline-safe) + apply, then kick a background refresh and
   * start the periodic timer. */
  start(): Promise<void>;
  /** Fetch + verify + apply once. Never throws. */
  refresh(): Promise<void>;
  /** Stop the periodic timer. */
  stop(): void;
}

const policiesKind: ConfigKind<RedactionPolicyBundle> = POLICIES_CONFIG_KIND;

/**
 * Build a refresher for the signed `policies` augment. Construct it once at
 * daemon startup (next to the heartbeat/discovery timers) and call `start()`.
 */
export function createPolicyRefresher(opts: PolicyRefresherOptions): PolicyRefresher {
  const env: RemoteConfigEnv = {
    apiUrl: opts.apiUrl,
    cache: createNodeDiskCache(opts.cacheDir ? { dir: opts.cacheDir } : {}),
    ...(opts.fetch ? { fetch: opts.fetch } : {}),
    ...(opts.logger ? { logger: opts.logger } : {}),
  };
  const store = new RemoteConfigStore(env, [policiesKind]);
  let timer: ReturnType<typeof setInterval> | null = null;

  const apply = (): void => {
    const bundle = store.get<RedactionPolicyBundle>("policies");
    setRemoteRedactionPatterns(compilePolicyPatterns(bundle));
  };

  const refresh = async (): Promise<void> => {
    await store.refresh("policies");
    apply();
  };

  return {
    async start(): Promise<void> {
      await store.initFromCache(); // instant, offline-safe (disk → bundled)
      apply();
      void refresh().catch(() => {}); // first network refresh in the background
      timer = setInterval(
        () => void refresh().catch(() => {}),
        opts.intervalMs ?? DEFAULT_REFRESH_MS,
      );
      timer.unref?.();
    },
    refresh,
    stop(): void {
      if (timer) clearInterval(timer);
      timer = null;
    },
  };
}
