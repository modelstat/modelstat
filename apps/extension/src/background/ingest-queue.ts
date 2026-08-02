/**
 * Ingest queue — drains unsynced events to the modelstat API.
 *
 * Triggered by an alarm every INGEST_BATCH_INTERVAL_MS. Groups unsynced
 * events into batches of ≤ INGEST_BATCH_MAX_EVENTS, serialises as an
 * `IngestBatch`, POSTs to /v1/ingest. Marks rows synced on 200;
 * exponential backoff on 5xx; drops on 400 (schema validation failure
 * — log loud so we notice).
 */

import { IngestBatch, type RawEvent } from "@modelstat/core/schemas";
import {
  DAEMON_VERSION,
  DEFAULT_API_URL,
  FORCE_SHIP_THRESHOLD,
  INGEST_BATCH_INTERVAL_MS,
  INGEST_BATCH_MAX_EVENTS,
  SESSION_DEBOUNCE_MS,
  ulid,
} from "@/common/config.js";
import { createLogger } from "@/common/logger.js";
import { db, getSetting, setSetting, type StoredEvent } from "@/storage/db.js";
import { getBearerToken, getDeviceId } from "./auth.js";
import { bump } from "./counters.js";
import { buildSegments } from "./pipeline.js";

const log = createLogger("ingest");

function rawEventFromStored(typed: StoredEvent): RawEvent {
  return {
    source_event_id: typed.source_event_id,
    ts: typed.ts,
    kind: typed.role === "user" ? "user_message" : "assistant_message",
    agent: typed.agent,
    provider: typed.vendor as unknown as RawEvent["provider"],
    model: typed.model,
    session_id: typed.session_id,
    turn_index: null,
    parent_event_id: null,
    cwd: null,
    git: null,
    tokens: {
      input: typed.input_tokens,
      output: typed.output_tokens,
      cache_creation: typed.cache_creation_tokens,
      cache_read: typed.cache_read_tokens,
      reasoning: typed.reasoning_tokens,
    },
    duration_ms: typed.duration_ms,
    // Web transcripts carry no tool-call data yet (claude.ai /
    // chatgpt.com captures expose text turns only), so the aggregate
    // map stays empty and this path never populates
    // IngestBatch.tool_calls either. When a capture source starts
    // exposing tool activity, mirror the CLI plumbing:
    // QueueItem.tool_calls → buildBatches → attachSegmentIds
    // (@modelstat/daemon-core/queue).
    tool_calls: {},
    files_touched: [],
    source_file: null,
    source_byte_offset: null,
    // Always a subscription, and not a guess: this extension captures web
    // chat only (claude_web / chatgpt_web / gemini_web / grok_web), and the
    // web UIs have no metered path — you are on a plan (paid or free) or you
    // are not talking to them at all. There is no `OPENAI_API_KEY` route
    // through chatgpt.com, so unlike the CLI parsers there is nothing here to
    // observe and nothing to be uncertain about.
    pricing_mode: "subscription",
  };
}

export async function flushQueue(opts?: {
  eager?: boolean;
}): Promise<{ sent: number; remaining: number }> {
  const syncEnabled = await getSetting<boolean>("syncEnabled", true);
  if (!syncEnabled) return { sent: 0, remaining: 0 };

  // First-impression fast path: until this device has shipped once, an eager
  // flush ignores the session debounce so the very first session reaches the
  // dashboard immediately. After that flip, every flush batches normally.
  const warmedUp = await getSetting<boolean>("firstShipDone", false);
  const eager = !!opts?.eager && !warmedUp;

  const deviceId = await getDeviceId();
  const token = await getBearerToken();
  // Self-registered devices have a device_secret even while unclaimed
  // — they can still ingest into their pending org.
  if (!deviceId || !token) return { sent: 0, remaining: 0 };

  const apiUrl = (await getSetting<string>("apiUrl", DEFAULT_API_URL)) || DEFAULT_API_URL;
  const allUnsynced = await db()
    .events.where("synced")
    .equals(0 as unknown as number)
    .toArray();
  if (allUnsynced.length === 0) return { sent: 0, remaining: 0 };

  // Group by session_id. For each session decide: ship it now, or
  // hold (because the session is still being written to within the
  // debounce window).
  type Group = { session_id: string; rows: StoredEvent[]; lastTs: number };
  const groups = new Map<string, Group>();
  for (const e of allUnsynced) {
    const ts = new Date(e.ts).getTime();
    const g = groups.get(e.session_id);
    if (g) {
      g.rows.push(e);
      if (ts > g.lastTs) g.lastTs = ts;
    } else {
      groups.set(e.session_id, { session_id: e.session_id, rows: [e], lastTs: ts });
    }
  }

  // Ready = session quiet for ≥ SESSION_DEBOUNCE_MS. Order by lastTs
  // descending so most-recent conversations ship first (the user's
  // current focus appears in the dashboard immediately; idle ones
  // drain after).
  const cutoff = eager ? Date.now() : Date.now() - SESSION_DEBOUNCE_MS;
  const ready = Array.from(groups.values())
    .filter((g) => g.lastTs <= cutoff)
    .sort((a, b) => b.lastTs - a.lastTs);

  // Also let a session ship if it has accumulated FORCE_SHIP_THRESHOLD
  // pending rows — don't hold back a long-running conversation forever
  // just because it keeps emitting new messages within the debounce.
  for (const g of groups.values()) {
    if (g.rows.length >= FORCE_SHIP_THRESHOLD && !ready.includes(g)) ready.push(g);
  }

  const rows: StoredEvent[] = [];
  for (const g of ready) {
    for (const r of g.rows) {
      rows.push(r);
      if (rows.length >= INGEST_BATCH_MAX_EVENTS) break;
    }
    if (rows.length >= INGEST_BATCH_MAX_EVENTS) break;
  }
  if (rows.length === 0) return { sent: 0, remaining: allUnsynced.length };

  // Run the shared daemon pipeline — redact → segment → summarise
  // → tag — against the offscreen document's adapters (MiniLM embed,
  // Chrome Prompt API / WebLLM summariser, tiktoken). Segments ship
  // alongside events so the server sees the exact same shape it gets
  // from the CLI via Ollama.
  const events = rows.map(rawEventFromStored);
  let segments: Awaited<ReturnType<typeof buildSegments>> = [];
  try {
    segments = await buildSegments(events);
  } catch (e) {
    log.warn("pipeline failed — shipping events only", e);
  }
  const batch = IngestBatch.parse({
    batch_id: ulid(),
    device_id: deviceId,
    daemon_version: DAEMON_VERSION,
    events,
    segments,
  });

  let response: Response;
  try {
    response = await fetch(`${apiUrl}/v1/ingest`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify(batch),
    });
  } catch (e) {
    log.warn("ingest network error — will retry", e);
    return { sent: 0, remaining: rows.length };
  }

  if (response.status === 400) {
    const body = await response.text();
    log.error("ingest rejected schema", body);
    // Mark these events synced to avoid a poison-pill loop; keep raw
    // copy in a "dead-letter" table in a follow-up.
    for (const r of rows) {
      if (r.id !== undefined) await db().events.update(r.id, { synced: 1 });
    }
    return { sent: 0, remaining: 0 };
  }

  if (!response.ok) {
    log.warn(`ingest ${response.status} — backing off`);
    return { sent: 0, remaining: rows.length };
  }

  for (const r of rows) {
    if (r.id !== undefined) await db().events.update(r.id, { synced: 1 });
  }
  // The device's first data has landed server-side — leave the first-impression
  // fast path for good; every later session uses the efficient batched cadence.
  if (!warmedUp) await setSetting("firstShipDone", true);
  bump("ingested", rows.length);
  const remaining = await db()
    .events.where("synced")
    .equals(0 as unknown as number)
    .count();
  log.info(`flushed ${rows.length} events, ${remaining} remain`);
  return { sent: rows.length, remaining };
}

export function setupIngestAlarm(): void {
  chrome.alarms.create("modelstat-flush-ingest", {
    periodInMinutes: INGEST_BATCH_INTERVAL_MS / 60_000,
  });
}
