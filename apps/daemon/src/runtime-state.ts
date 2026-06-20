/**
 * Per-install runtime state — file cursors, the resolved API URL override, the
 * lifetime segments-sent counter, and the processing-pipeline version marker.
 *
 * Lives at `<modelstatHome()>/state.json` (default `~/.modelstat/state.json`),
 * right next to `identity.json`. This replaces the old `conf` store, which lived
 * at an OS- and project-name-specific path (`~/Library/Preferences/
 * modelstat-agent-dev-nodejs/…`) — so a rename or a dev build orphaned cursors
 * into a second file. Now there is exactly one location, identical on every
 * platform, relocatable as a unit via `MODELSTAT_HOME`.
 *
 * Identity stays in `identity.json` (long-lived bearer, chmod 0600). This file
 * is non-secret per-install bookkeeping; losing it just re-scans transcripts.
 */

import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { modelstatHome, homePath } from "./paths.js";

export interface FileCursor {
  size: number;
  mtime: number;
  tailHash: string;
}

export interface RuntimeState {
  /** Operator-set API URL override (empty ⇒ env / production default). */
  apiUrl: string;
  /** Per-file scan cursors, keyed by absolute transcript path. */
  cursor: Record<string, FileCursor>;
  /** Lifetime count of segments uploaded from this machine (cosmetic). */
  segmentsSent: number;
  /** Pipeline version that last produced the cursors; bump ⇒ full re-scan. */
  processingVersion: number | null;
}

const DEFAULTS: RuntimeState = {
  apiUrl: "",
  cursor: {},
  segmentsSent: 0,
  processingVersion: null,
};

export function statePath(): string {
  return homePath("state.json");
}

// In-memory cache so getters don't re-read the file each access; setters write
// through to both the cache and disk.
let cache: RuntimeState | null = null;

function load(): RuntimeState {
  if (cache) return cache;
  try {
    const obj = JSON.parse(readFileSync(statePath(), "utf8")) as Partial<RuntimeState>;
    cache = {
      apiUrl: obj.apiUrl ?? DEFAULTS.apiUrl,
      cursor: obj.cursor ?? {},
      segmentsSent: obj.segmentsSent ?? 0,
      processingVersion: obj.processingVersion ?? null,
    };
  } catch {
    // Missing/corrupt ⇒ fresh defaults (a new install, or post-`MODELSTAT_HOME`
    // relocation). Cursors start empty, so processingVersion=null forces a
    // re-scan on first run — exactly what we want.
    cache = { ...DEFAULTS, cursor: {} };
  }
  return cache;
}

function persist(s: RuntimeState): void {
  mkdirSync(modelstatHome(), { recursive: true, mode: 0o700 });
  const tmp = `${statePath()}.${process.pid}.tmp`;
  writeFileSync(tmp, JSON.stringify(s, null, 2), { mode: 0o600 });
  renameSync(tmp, statePath());
}

export const runtimeState = {
  getApiUrl(): string {
    return load().apiUrl;
  },
  setApiUrl(v: string): void {
    const s = load();
    s.apiUrl = v;
    persist(s);
  },

  getCursor(path: string): FileCursor | undefined {
    return load().cursor[path];
  },
  setCursor(path: string, v: FileCursor): void {
    const s = load();
    s.cursor[path] = v;
    persist(s);
  },
  /** Drop every per-file cursor so the next scan re-reads each JSONL from byte 0
   * (processing-version bump, or `modelstat rescan`). */
  wipeCursors(): void {
    const s = load();
    s.cursor = {};
    persist(s);
  },

  getSegmentsSent(): number {
    return load().segmentsSent;
  },
  bumpSegmentsSent(n: number): number {
    const s = load();
    s.segmentsSent += n;
    persist(s);
    return s.segmentsSent;
  },

  getProcessingVersion(): number | null {
    return load().processingVersion;
  },
  setProcessingVersion(v: number): void {
    const s = load();
    s.processingVersion = v;
    persist(s);
  },

  /** Test-only: drop the in-memory cache so the next read hits disk. */
  _resetCacheForTests(): void {
    cache = null;
  },
};
