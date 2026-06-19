/**
 * Live capture counters, exposed to the popup. Lets users (and us)
 * see at a glance whether the pipeline is flowing:
 *
 *   frames      — MAIN-world intercepts of fetch/XHR (total)
 *   matches     — network frames whose URL matched an adapter regex
 *   dom_events  — DOM observer emissions from content script
 *   messages    — messages extracted by the interpreter
 *   events      — finalised events written to IndexedDB
 *   heartbeats  — successful /v1/daemon/heartbeat posts
 *   ingested    — events synced to the modelstat API
 *
 * These live in-memory on the SW; lost on SW eviction. That's fine —
 * they're a live activity indicator, not durable state.
 */

export type Counters = {
  frames: number;
  matches: number;
  dom_events: number;
  messages: number;
  events: number;
  heartbeats: number;
  ingested: number;
  last_frame_at: number | null;
  last_event_at: number | null;
};

export const counters: Counters = {
  frames: 0,
  matches: 0,
  dom_events: 0,
  messages: 0,
  events: 0,
  heartbeats: 0,
  ingested: 0,
  last_frame_at: null,
  last_event_at: null,
};

/** Ring buffer of the last N distinct captured URLs (host+path only,
 * query stripped). Shown in the popup so the user / we can see what
 * the MAIN-world patch is intercepting — invaluable when `matches` is
 * stuck at 0 because the adapter regex doesn't fit the current
 * provider paths. No query strings (may contain IDs). */
const MAX_RECENT_URLS = 12;
const recentUrls: Array<{ url: string; method: string; matched: boolean; at: number }> = [];

export function noteUrl(url: string, method: string, matched: boolean): void {
  let clean: string;
  try {
    const u = new URL(url);
    clean = `${u.host}${u.pathname}`;
  } catch {
    clean = url.slice(0, 120);
  }
  const existing = recentUrls.findIndex((r) => r.url === clean && r.method === method);
  if (existing >= 0) {
    const row = recentUrls[existing]!;
    row.at = Date.now();
    row.matched = row.matched || matched;
    return;
  }
  recentUrls.unshift({ url: clean, method, matched, at: Date.now() });
  if (recentUrls.length > MAX_RECENT_URLS) recentUrls.length = MAX_RECENT_URLS;
}

export function recentUrlsSnapshot(): typeof recentUrls {
  return recentUrls.slice();
}

/** Per-tab last-frame timestamp. Lets the popup tell whether THIS
 * specific tab is attached (its MAIN-world patch running) vs the
 * global "any frame captured lately" signal. Memory-bounded via a
 * 32-tab LRU — the SW dies after 30s idle anyway. */
const tabLastFrameAt = new Map<number, number>();
const MAX_TRACKED_TABS = 32;

export function bump<K extends keyof Counters>(k: K, by = 1): void {
  if (typeof counters[k] === "number") (counters[k] as number) += by;
  if (k === "frames") counters.last_frame_at = Date.now();
  if (k === "events") counters.last_event_at = Date.now();
}

export function noteTabFrame(tabId: number): void {
  tabLastFrameAt.set(tabId, Date.now());
  if (tabLastFrameAt.size > MAX_TRACKED_TABS) {
    const oldest = tabLastFrameAt.keys().next().value;
    if (oldest !== undefined) tabLastFrameAt.delete(oldest);
  }
}

export function tabLastFrame(tabId: number): number | null {
  return tabLastFrameAt.get(tabId) ?? null;
}
