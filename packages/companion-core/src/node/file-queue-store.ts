/**
 * FileQueueStore — zero-dependency QueueStore for the CLI companion.
 *
 * Persists queue state as a single JSON document next to the agent's
 * Conf state file. Every mutation writes atomically via a temp-file +
 * rename so a crash mid-write can't corrupt the snapshot.
 *
 * Scale assumption: the agent holds at most a few thousand unsynced
 * events at any time (the upload loop drains them every few seconds).
 * At that size an in-memory Map + atomic JSON rewrite is simpler, more
 * portable, and crash-safer than embedding a native SQLite binding
 * (see the deprecation trail on prebuild-install → better-sqlite3).
 *
 * Format on disk (v2):
 *   {
 *     "version": 2,
 *     "items": { [sourceEventId]: QueueItem }
 *   }
 *
 * On load we drop any items with synced=true older than a day — once a
 * batch is accepted, keeping its provenance past the idempotency
 * window doesn't help; older agents treated queue_items as an append-
 * only log and it grew unbounded.
 *
 * Snapshot version history:
 *   v1 → v2: QueueItem.tool renamed to QueueItem.agent (AGENTS rename).
 *            v1 snapshots carry the old `tool` key, so they're dropped
 *            wholesale on load (nothing is in production; re-scanning
 *            re-enqueues). Migration is intentionally lossy.
 */
import { promises as fs } from "node:fs";
import { dirname } from "node:path";
import type { QueueItem, QueueStore } from "../queue/index.js";

interface Snapshot {
  version: 2;
  items: Record<string, QueueItem>;
}

const SENT_TTL_MS = 24 * 60 * 60 * 1000;

export class FileQueueStore implements QueueStore {
  private readonly filePath: string;
  private items = new Map<string, QueueItem>();
  private loaded = false;
  private writing: Promise<void> | null = null;

  constructor(filePath: string) {
    this.filePath = filePath;
  }

  /** Lazily load the snapshot on first access so construction stays
   * synchronous (matches the old SqliteQueueStore contract). */
  private async ensureLoaded(): Promise<void> {
    if (this.loaded) return;
    try {
      const raw = await fs.readFile(this.filePath, "utf8");
      const parsed = JSON.parse(raw) as Snapshot;
      if (parsed.version === 2 && parsed.items) {
        const now = Date.now();
        for (const [k, v] of Object.entries(parsed.items)) {
          if (v.synced && now - v.last_event_ts_ms > SENT_TTL_MS) continue;
          this.items.set(k, v);
        }
      }
      // Older-versioned snapshots (v1: pre-AGENTS-rename `tool` key) are
      // dropped wholesale — the map stays empty and the next mutation
      // overwrites the file atomically.
    } catch (err) {
      // Missing file on first boot is expected. Any other error: leave
      // the in-memory map empty — next mutation will overwrite the bad
      // file atomically.
      if ((err as NodeJS.ErrnoException).code !== "ENOENT") {
        // Corrupt snapshot — try once with the .bak if present.
        try {
          const bak = await fs.readFile(`${this.filePath}.bak`, "utf8");
          const parsed = JSON.parse(bak) as Snapshot;
          if (parsed.version === 2 && parsed.items) {
            for (const [k, v] of Object.entries(parsed.items)) {
              this.items.set(k, v);
            }
          }
        } catch {
          /* fall through with an empty map */
        }
      }
    }
    this.loaded = true;
  }

  /** Serialize writes so overlapping mutations don't race on the
   * temp-file / rename swap. writing is a single-slot promise; each
   * caller awaits the previous write and then enqueues its own. */
  private async persist(): Promise<void> {
    const prior = this.writing;
    const run = async () => {
      await prior?.catch(() => undefined);
      const snap: Snapshot = {
        version: 2,
        items: Object.fromEntries(this.items.entries()),
      };
      const tmp = `${this.filePath}.tmp`;
      await fs.mkdir(dirname(this.filePath), { recursive: true });
      await fs.writeFile(tmp, JSON.stringify(snap), "utf8");
      // Keep a single-generation .bak so a corrupt .tmp promote doesn't
      // orphan us. Best-effort: ignore ENOENT when there's no prior.
      try {
        await fs.rename(this.filePath, `${this.filePath}.bak`);
      } catch (err) {
        if ((err as NodeJS.ErrnoException).code !== "ENOENT") throw err;
      }
      await fs.rename(tmp, this.filePath);
    };
    this.writing = run();
    try {
      await this.writing;
    } finally {
      this.writing = null;
    }
  }

  async put(item: QueueItem): Promise<void> {
    await this.ensureLoaded();
    if (this.items.has(item.source_event_id)) return;
    this.items.set(item.source_event_id, item);
    await this.persist();
  }

  async has(sourceEventId: string): Promise<boolean> {
    await this.ensureLoaded();
    return this.items.has(sourceEventId);
  }

  async listUnsentBySession(): Promise<Map<string, QueueItem[]>> {
    await this.ensureLoaded();
    const out = new Map<string, QueueItem[]>();
    const sorted = [...this.items.values()]
      .filter((i) => !i.synced)
      .sort((a, b) => {
        if (a.session_id !== b.session_id)
          return a.session_id < b.session_id ? -1 : 1;
        return a.last_event_ts_ms - b.last_event_ts_ms;
      });
    for (const it of sorted) {
      const arr = out.get(it.session_id) ?? [];
      arr.push(it);
      out.set(it.session_id, arr);
    }
    return out;
  }

  async markSent(sourceEventIds: string[], batchId: string | null): Promise<void> {
    if (sourceEventIds.length === 0) return;
    await this.ensureLoaded();
    for (const id of sourceEventIds) {
      const it = this.items.get(id);
      if (!it) continue;
      it.synced = true;
      it.sent_batch_id = batchId;
    }
    await this.persist();
  }

  async countUnsent(): Promise<number> {
    await this.ensureLoaded();
    let n = 0;
    for (const it of this.items.values()) if (!it.synced) n++;
    return n;
  }

  /** Compat stub — Sqlite store exposed close(); nothing to release here. */
  close(): void {
    /* no-op */
  }
}
