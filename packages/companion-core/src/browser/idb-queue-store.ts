/**
 * IdbQueueStore — IndexedDB-backed QueueStore for the Chrome extension.
 * Mirrors the SqliteQueueStore shape on Node so the shared state
 * machine behaves identically on both companions.
 *
 * Object store: `queue_items`
 * Indexes:
 *   - session_unsent: [session_id, last_event_ts_ms]   where synced=0
 *   - unsent:         synced
 *
 * Uses Storage Foundation via the idb-kit-free API (no deps). The
 * existing extension wiring already uses IndexedDB directly
 * (apps/extension/src/background/db.ts), so a store handle from there
 * can be passed in at construction time to avoid duplicating db setup.
 */
import type { RawEvent } from "@modelstat/core/schemas";
import type { QueueItem, QueueStore, ToolCallDraft } from "../queue/index.js";

const STORE_NAME = "queue_items";
const IDX_UNSENT_SESSION = "unsent_session";
const IDX_UNSENT = "unsent";

interface StoredRow {
  source_event_id: string;
  session_id: string;
  agent: string;
  event_json: string;
  last_event_ts_ms: number;
  /** 0/1 — kept numeric so it's an indexable IDBKey. */
  synced: 0 | 1;
  sent_batch_id: string | null;
  /** Per-call tool invocations born from this event (same privacy
   * contract as ToolCallWire). Persisted so a browser-queued call
   * survives the round-trip — mirrors FileQueueStore, which JSON-
   * serializes the whole QueueItem (including tool_calls). Absent on
   * rows written before this field existed and on runtimes that carry
   * no tool data. */
  tool_calls?: ToolCallDraft[];
}

/** Open (or create) the queue_items store on the passed IDB database.
 * Call this inside the shared extension DB's `onupgradeneeded` if you
 * want a single schema version; or pass a new IDBDatabase scoped to
 * the queue alone. */
export function ensureQueueStore(db: IDBDatabase): void {
  if (db.objectStoreNames.contains(STORE_NAME)) return;
  const store = db.createObjectStore(STORE_NAME, { keyPath: "source_event_id" });
  store.createIndex(IDX_UNSENT_SESSION, ["session_id", "last_event_ts_ms"]);
  store.createIndex(IDX_UNSENT, "synced");
}

export class IdbQueueStore implements QueueStore {
  constructor(private readonly db: IDBDatabase) {}

  async put(item: QueueItem): Promise<void> {
    const row: StoredRow = {
      source_event_id: item.source_event_id,
      session_id: item.session_id,
      agent: item.agent,
      event_json: JSON.stringify(item.event),
      last_event_ts_ms: item.last_event_ts_ms,
      synced: 0,
      sent_batch_id: null,
      // Only persist when present so old rows / tool-less runtimes stay
      // free of the key (IndexedDB stores the structured clone as-is).
      ...(item.tool_calls ? { tool_calls: item.tool_calls } : {}),
    };
    await this.tx("readwrite", (store) => store.add(row)).catch((e) => {
      // ConstraintError on duplicate key → dedupe no-op.
      if ((e as DOMException).name !== "ConstraintError") throw e;
    });
  }

  async has(sourceEventId: string): Promise<boolean> {
    const n = await this.tx("readonly", (store) => store.count(sourceEventId));
    return n > 0;
  }

  async listUnsentBySession(): Promise<Map<string, QueueItem[]>> {
    const rows = await this.tx("readonly", (store) => {
      const idx = store.index(IDX_UNSENT);
      // IDBKeyRange.only(0) — all unsent rows.
      return idx.getAll(IDBKeyRange.only(0)) as IDBRequest<StoredRow[]>;
    });
    const out = new Map<string, QueueItem[]>();
    for (const r of rows as StoredRow[]) {
      const arr = out.get(r.session_id) ?? [];
      arr.push({
        source_event_id: r.source_event_id,
        session_id: r.session_id,
        agent: r.agent,
        event: JSON.parse(r.event_json) as RawEvent,
        last_event_ts_ms: r.last_event_ts_ms,
        synced: r.synced === 1,
        sent_batch_id: r.sent_batch_id,
        // Restore tool_calls so buildBatches → attachSegmentIds sees them.
        // Omit the key entirely when the stored row lacks it (old rows /
        // tool-less runtimes) to match QueueItem's optional shape.
        ...(r.tool_calls ? { tool_calls: r.tool_calls } : {}),
      });
      out.set(r.session_id, arr);
    }
    // Deterministic session ordering for the state machine.
    const sorted = new Map<string, QueueItem[]>();
    for (const [sid, items] of [...out].sort(([a], [b]) => a.localeCompare(b))) {
      items.sort((x, y) => x.last_event_ts_ms - y.last_event_ts_ms);
      sorted.set(sid, items);
    }
    return sorted;
  }

  async markSent(sourceEventIds: string[], batchId: string | null): Promise<void> {
    if (sourceEventIds.length === 0) return;
    await new Promise<void>((resolve, reject) => {
      const tx = this.db.transaction(STORE_NAME, "readwrite");
      const store = tx.objectStore(STORE_NAME);
      for (const id of sourceEventIds) {
        const req = store.get(id);
        req.onsuccess = () => {
          const row = req.result as StoredRow | undefined;
          if (!row) return;
          row.synced = 1;
          row.sent_batch_id = batchId;
          store.put(row);
        };
      }
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
    });
  }

  async countUnsent(): Promise<number> {
    return this.tx("readonly", (store) =>
      store.index(IDX_UNSENT).count(IDBKeyRange.only(0)),
    );
  }

  // ── internal helpers ─────────────────────────────────────────

  private tx<T>(
    mode: IDBTransactionMode,
    fn: (store: IDBObjectStore) => IDBRequest<T>,
  ): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const t = this.db.transaction(STORE_NAME, mode);
      const store = t.objectStore(STORE_NAME);
      const req = fn(store);
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  }
}
