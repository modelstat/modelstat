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
import { homePath, modelstatHome } from "./paths.js";

export interface FileCursor {
  size: number;
  mtime: number;
  tailHash: string;
}

/**
 * Where each session gets summarised — chosen at install, changeable later via
 * `modelstat mode`. Redaction (secrets + on-device NER/PII + emails + paths)
 * runs client-side in EVERY mode; only the summarisation LOCATION differs.
 *
 * - `local`       — the bundled Qwen model summarises on THIS machine (the only
 *   mode that downloads/loads the ~2.7 GB model). Ships abstracts to /v1/ingest.
 * - `self-hosted` — an org-run OpenAI-compatible endpoint summarises the cleaned
 *   excerpts (see selfHostedUrl/selfHostedModel). Ships abstracts to /v1/ingest.
 * - `cloud`       — no local summariser; cleaned turns ship to /v1/ingest/raw
 *   and modelstat's cloud summarises server-side. The install default.
 */
export type SummarizerMode = "local" | "self-hosted" | "cloud";

/** All valid modes, in menu order (Cloud first — it's the default). */
export const SUMMARIZER_MODES: readonly SummarizerMode[] = ["cloud", "local", "self-hosted"];

/** The install default: cloud (no local model, server-side summarisation). */
export const DEFAULT_SUMMARIZER_MODE: SummarizerMode = "cloud";

/** Narrow an arbitrary string to a {@link SummarizerMode}, else `null`. */
export function parseSummarizerMode(v: string | null | undefined): SummarizerMode | null {
  const s = (v ?? "").trim().toLowerCase();
  return (SUMMARIZER_MODES as readonly string[]).includes(s) ? (s as SummarizerMode) : null;
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
  /** Self-healing reconcile cache: per transcript file, its mtime and a
   * (UTC-day → session → event-count) tally, so reconcile compares against the
   * server WITHOUT re-parsing unchanged files (O(changed files)). Bounded by the
   * CURRENT file set (GC'd as files vanish) — flat over months. */
  reconcileCache: Record<
    string,
    { mtime: number; perDaySession: Record<string, Record<string, number>> }
  >;
  /** Did the last run ship extractive (LLM-unavailable) abstracts? When the
   * bundled summariser is healthy again the daemon re-scans so those degraded
   * abstracts upgrade to model quality. See apps/daemon/src/pipeline.ts. */
  summariserDegraded: boolean;
  /** ms-epoch of the last degradation-recovery re-scan — bounds re-scans on a
   * flaky LLM to at most once per window so a preflight-passes/scan-fails loop
   * can't re-scan the world every restart. */
  summariserRecoveryAt: number;
  /** Per-(day,session) self-heal re-ship bookkeeping, keyed `${day}\0${sid}`.
   * `attempts` is how many times we've re-shipped this session that the server
   * still reports short; `lastAt` is the ms-epoch of the last re-ship. Together
   * they drive an exponential backoff so a session the server DEDUPES (re-ship
   * never reduces the reported deficit) stops re-shipping every pass instead of
   * looping forever. GC'd to the current (day,session) set each pass. */
  reshipState: Record<string, { attempts: number; lastAt: number }>;
  /** Where sessions get summarised (chosen at install; see {@link SummarizerMode}).
   * Defaults to `cloud`. */
  summarizerMode: SummarizerMode;
  /** Self-hosted only: the org's OpenAI-compatible summariser base URL (e.g.
   * `https://llm.acme.internal/v1`). Empty in local/cloud mode. */
  selfHostedUrl: string;
  /** Self-hosted only: the model id to request from {@link selfHostedUrl}
   * (e.g. `qwen2.5-7b-instruct`). Empty in local/cloud mode. */
  selfHostedModel: string;
}

const DEFAULTS: RuntimeState = {
  apiUrl: "",
  cursor: {},
  segmentsSent: 0,
  processingVersion: null,
  reconcileCache: {},
  summariserDegraded: false,
  summariserRecoveryAt: 0,
  reshipState: {},
  summarizerMode: DEFAULT_SUMMARIZER_MODE,
  selfHostedUrl: "",
  selfHostedModel: "",
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
      reconcileCache: obj.reconcileCache ?? {},
      summariserDegraded: obj.summariserDegraded ?? false,
      summariserRecoveryAt: obj.summariserRecoveryAt ?? 0,
      reshipState: obj.reshipState ?? {},
      // Validate the persisted mode — a hand-edited/garbage value falls back to
      // the default rather than driving the pipeline off a bogus branch.
      summarizerMode: parseSummarizerMode(obj.summarizerMode) ?? DEFAULT_SUMMARIZER_MODE,
      selfHostedUrl: obj.selfHostedUrl ?? "",
      selfHostedModel: obj.selfHostedModel ?? "",
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
   * (processing-version bump, or `modelstat reset`). */
  wipeCursors(): void {
    const s = load();
    s.cursor = {};
    persist(s);
  },
  /** Drop ONE file's cursor so the next scan re-reads it — the precise lever the
   * self-healing reconcile pulls for the files of sessions the server is missing. */
  clearCursor(path: string): void {
    const s = load();
    if (path in s.cursor) {
      delete s.cursor[path];
      persist(s);
    }
  },
  /** Drop cursors for files no longer present so the map tracks the CURRENT file
   * set, not every file ever seen. Returns how many were pruned. */
  pruneCursors(present: Set<string>): number {
    const s = load();
    let removed = 0;
    for (const p of Object.keys(s.cursor)) {
      if (!present.has(p)) {
        delete s.cursor[p];
        removed += 1;
      }
    }
    if (removed) persist(s);
    return removed;
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

  /** Whether the last run shipped extractive (LLM-unavailable) abstracts. */
  getSummariserDegraded(): boolean {
    return load().summariserDegraded;
  },
  setSummariserDegraded(v: boolean): void {
    const s = load();
    if (s.summariserDegraded === v) return; // no-op write avoidance (hot path)
    s.summariserDegraded = v;
    persist(s);
  },
  /** ms-epoch of the last degradation-recovery re-scan (0 = never). */
  getSummariserRecoveryAt(): number {
    return load().summariserRecoveryAt;
  },
  setSummariserRecoveryAt(ms: number): void {
    const s = load();
    s.summariserRecoveryAt = ms;
    persist(s);
  },

  /** Self-healing reconcile cache (see {@link RuntimeState.reconcileCache}). */
  getReconcileCache(): RuntimeState["reconcileCache"] {
    return load().reconcileCache;
  },
  setReconcileCache(c: RuntimeState["reconcileCache"]): void {
    const s = load();
    s.reconcileCache = c;
    persist(s);
  },

  /** Per-(day,session) re-ship backoff bookkeeping (see
   * {@link RuntimeState.reshipState}). */
  getReshipState(): RuntimeState["reshipState"] {
    return load().reshipState;
  },
  setReshipState(r: RuntimeState["reshipState"]): void {
    const s = load();
    s.reshipState = r;
    persist(s);
  },

  /** The persisted summariser mode (see {@link SummarizerMode}). */
  getSummarizerMode(): SummarizerMode {
    return load().summarizerMode;
  },
  setSummarizerMode(v: SummarizerMode): void {
    const s = load();
    s.summarizerMode = v;
    persist(s);
  },
  /** Self-hosted endpoint config (base URL + model id). Both empty unless the
   * mode is `self-hosted`. */
  getSelfHosted(): { url: string; model: string } {
    const s = load();
    return { url: s.selfHostedUrl, model: s.selfHostedModel };
  },
  setSelfHosted(url: string, model: string): void {
    const s = load();
    s.selfHostedUrl = url;
    s.selfHostedModel = model;
    persist(s);
  },

  /** Test-only: drop the in-memory cache so the next read hits disk. */
  _resetCacheForTests(): void {
    cache = null;
  },
};
