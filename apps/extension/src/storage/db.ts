/**
 * IndexedDB schema for the extension. Dexie wrapper.
 *
 * Tables:
 *   events        — finalised RawEvent-shaped rows
 *   sessions      — per-conversation rollups
 *   pending       — in-flight messages awaiting the two-phase commit
 *                   window to close
 *   settings      — single-row key/value store
 *   queue         — ingest batches awaiting upload
 *   breakage      — canary invariant failures pending telemetry flush
 */

import Dexie, { type Table } from "dexie";
import type { Agent } from "@modelstat/core/enums";

export type PendingMessage = {
  // Primary key: (host, messageId)
  key: string;
  host: string;
  messageId: string;
  role: "user" | "assistant";
  text: string | null;
  model: string | null;
  conversationId: string | null;
  usage: {
    input: number | null;
    output: number | null;
    cache_creation: number | null;
    cache_read: number | null;
    reasoning: number | null;
  };
  firstSeenAt: number;
  lastUpdatedAt: number;
  streamEnded: boolean;
  domStableSince: number | null;
  finalised: boolean;
};

export type StoredEvent = {
  id?: number;
  source_event_id: string; // dedupe key = (host, messageId)
  ts: string; // ISO
  agent: Agent;
  vendor: string;
  model: string | null;
  session_id: string;
  role: "user" | "assistant";
  input_tokens: number;
  output_tokens: number;
  cache_creation_tokens: number;
  cache_read_tokens: number;
  reasoning_tokens: number;
  tokenizer_name: string;
  tokenizer_accuracy: "exact" | "estimated";
  duration_ms: number | null;
  host: string;
  cost_usd: number | null;
  summary: string | null;
  category: string | null;
  synced: 0 | 1;
};

export type SessionRollup = {
  session_id: string; // (agent, conversationId)
  agent: Agent;
  vendor: string;
  model: string | null;
  conversation_id: string;
  host: string;
  started_at: string;
  ended_at: string;
  message_count: number;
  tokens_input: number;
  tokens_output: number;
  tokens_cache_creation: number;
  tokens_cache_read: number;
  tokens_reasoning: number;
  cost_usd: number;
  summary: string | null;
  category: string | null;
  updated_at: string;
};

export type QueueBatch = {
  id?: number;
  batch_id: string; // ULID
  ndjson: string; // pre-serialised IngestBatch
  created_at: number;
  attempts: number;
  next_attempt_at: number;
};

export type SettingRow = { key: string; value: unknown };

export type BreakageRow = {
  id?: number;
  provider: string;
  adapter_version: number;
  invariant: string;
  url_host: string;
  reported: 0 | 1;
  ts: number;
};

export class ExtensionDB extends Dexie {
  events!: Table<StoredEvent, number>;
  sessions!: Table<SessionRollup, string>;
  pending!: Table<PendingMessage, string>;
  queue!: Table<QueueBatch, number>;
  settings!: Table<SettingRow, string>;
  breakage!: Table<BreakageRow, number>;

  constructor() {
    super("modelstat_extension");
    this.version(1).stores({
      events: "++id, source_event_id, session_id, ts, synced, host, tool",
      sessions: "&session_id, tool, host, ended_at, updated_at",
      pending: "&key, host, finalised, lastUpdatedAt",
      queue: "++id, batch_id, next_attempt_at",
      settings: "&key",
      breakage: "++id, reported, ts",
    });
    // v2: StoredEvent.tool / SessionRollup.tool renamed to `agent`
    // (AGENTS rename). `tool` was an *indexed* column on both `events`
    // and `sessions`, so the index strings change. Nothing is in
    // production — the migration is intentionally lossy: clear both
    // tables rather than rewrite every row's key. The extension simply
    // re-derives events/rollups from live traffic after the upgrade.
    this.version(2)
      .stores({
        events: "++id, source_event_id, session_id, ts, synced, host, agent",
        sessions: "&session_id, agent, host, ended_at, updated_at",
      })
      .upgrade(async (tx) => {
        await tx.table("events").clear();
        await tx.table("sessions").clear();
      });
  }
}

let _db: ExtensionDB | null = null;
export function db(): ExtensionDB {
  if (!_db) _db = new ExtensionDB();
  return _db;
}

export async function getSetting<T>(key: string, fallback: T): Promise<T> {
  const row = await db().settings.get(key);
  return (row?.value as T | undefined) ?? fallback;
}

export async function setSetting(key: string, value: unknown): Promise<void> {
  await db().settings.put({ key, value });
}
