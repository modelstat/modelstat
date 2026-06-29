import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";
import type { RawEvent } from "@modelstat/core/schemas";
import type { QueueItem, ToolCallDraft } from "../queue/index.js";
import { IdbQueueStore } from "./idb-queue-store.js";

/**
 * Minimal in-memory IndexedDB shim.
 *
 * Node (the tsx test runner) has no IndexedDB, and the repo carries no
 * fake-indexeddb dependency. IdbQueueStore touches a small, well-defined
 * slice of the IDB surface — object-store add/get/getAll/count/put,
 * one transaction shape, two indexes, and the global IDBKeyRange.only —
 * so we fake exactly that, structured-clone semantics included (so a
 * stored row is decoupled from the live object, like real IDB).
 *
 * Records are keyed by `source_event_id` (the store's keyPath). The two
 * indexes are evaluated on demand:
 *   - "unsent": keyed on `synced`
 *   - "unsent_session": keyed on [session_id, last_event_ts_ms]
 * Only IDBKeyRange.only(value) is needed for lookups in this store.
 */

interface KeyRangeOnly {
  __only: unknown;
}

const FakeIDBKeyRange = {
  only(v: unknown): KeyRangeOnly {
    return { __only: v };
  },
};

function clone<T>(v: T): T {
  return structuredClone(v);
}

function rangeMatches(range: KeyRangeOnly | unknown, value: unknown): boolean {
  if (range && typeof range === "object" && "__only" in (range as KeyRangeOnly)) {
    return (range as KeyRangeOnly).__only === value;
  }
  // Bare key (used by count(key)).
  return range === value;
}

type Listenable<T> = {
  onsuccess: ((this: unknown) => void) | null;
  onerror: ((this: unknown) => void) | null;
  result: T;
  error: unknown;
};

function makeRequest<T>(compute: () => T): Listenable<T> {
  const req: Listenable<T> = {
    onsuccess: null,
    onerror: null,
    result: undefined as T,
    error: null,
  };
  // Resolve on the microtask queue so handlers assigned synchronously fire.
  queueMicrotask(() => {
    try {
      req.result = compute();
      req.onsuccess?.call(req);
    } catch (e) {
      req.error = e;
      req.onerror?.call(req);
    }
  });
  return req;
}

class FakeObjectStore {
  constructor(private readonly records: Map<string, Record<string, unknown>>) {}

  add(row: Record<string, unknown>) {
    return makeRequest(() => {
      const key = row.source_event_id as string;
      if (this.records.has(key)) {
        const err = new Error("ConstraintError");
        err.name = "ConstraintError";
        throw err;
      }
      this.records.set(key, clone(row));
      return undefined;
    });
  }

  put(row: Record<string, unknown>) {
    return makeRequest(() => {
      this.records.set(row.source_event_id as string, clone(row));
      return undefined;
    });
  }

  get(key: string) {
    return makeRequest(() => {
      const r = this.records.get(key);
      return r ? clone(r) : undefined;
    });
  }

  count(key?: unknown) {
    return makeRequest(() => {
      if (key === undefined) return this.records.size;
      let n = 0;
      for (const r of this.records.values()) if (r.source_event_id === key) n++;
      return n;
    });
  }

  index(name: string): FakeIndex {
    return new FakeIndex(name, this.records);
  }
}

class FakeIndex {
  constructor(
    private readonly name: string,
    private readonly records: Map<string, Record<string, unknown>>,
  ) {}

  private keyOf(r: Record<string, unknown>): unknown {
    return this.name === "unsent" ? r.synced : [r.session_id, r.last_event_ts_ms];
  }

  getAll(range: unknown) {
    return makeRequest(() => {
      const out: Record<string, unknown>[] = [];
      for (const r of this.records.values()) {
        if (rangeMatches(range, this.keyOf(r))) out.push(clone(r));
      }
      return out;
    });
  }

  count(range: unknown) {
    return makeRequest(() => {
      let n = 0;
      for (const r of this.records.values()) if (rangeMatches(range, this.keyOf(r))) n++;
      return n;
    });
  }
}

class FakeTransaction {
  onerror: ((this: unknown) => void) | null = null;
  oncomplete: ((this: unknown) => void) | null = null;
  error: unknown = null;
  constructor(private readonly records: Map<string, Record<string, unknown>>) {
    // Real IDB fires oncomplete after the request callbacks settle. One
    // microtask after this constructor is enough for the single-op flows
    // here; markSent issues N gets then relies on oncomplete, so push it
    // two ticks out to land after those request microtasks.
    queueMicrotask(() => queueMicrotask(() => this.oncomplete?.call(this)));
  }
  objectStore() {
    return new FakeObjectStore(this.records);
  }
}

class FakeDB {
  private readonly records = new Map<string, Record<string, unknown>>();
  transaction(_store: string, _mode?: string) {
    return new FakeTransaction(this.records);
  }
}

// Install the global IDBKeyRange the store reaches for.
(globalThis as unknown as { IDBKeyRange: unknown }).IDBKeyRange = FakeIDBKeyRange;

const TS = "2026-06-01T10:00:00.000Z";

function rawEvent(source_event_id: string, session_id: string): RawEvent {
  return {
    source_event_id,
    session_id,
    ts: TS,
    kind: "assistant_message",
    agent: "claude_code",
    provider: "anthropic",
    model: "claude-fable-5",
    turn_index: null,
    parent_event_id: null,
    cwd: null,
    git: null,
    tokens: null,
    duration_ms: null,
    tool_calls: {},
    files_touched: [],
    source_file: null,
    source_byte_offset: null,
  };
}

function draft(over: Partial<ToolCallDraft> & { external_call_id: string }): ToolCallDraft {
  return {
    session_id: "S",
    source_event_id: "e1",
    agent: "claude_code",
    server: "builtin",
    name: "Bash",
    turn_index: null,
    call_index: 0,
    started_at: TS,
    ended_at: null,
    status: "unknown",
    args_hash: "",
    signature_hash: "none",
    args_bytes: 0,
    result_bytes: 0,
    model: null,
    action: null,
    ...over,
  };
}

function item(
  over: Partial<QueueItem> & { source_event_id: string; session_id: string },
): QueueItem {
  return {
    agent: "claude_code",
    event: rawEvent(over.source_event_id, over.session_id),
    last_event_ts_ms: 0,
    synced: false,
    sent_batch_id: null,
    ...over,
  };
}

describe("IdbQueueStore tool_calls round-trip", () => {
  let store: IdbQueueStore;
  beforeEach(() => {
    store = new IdbQueueStore(new FakeDB() as unknown as IDBDatabase);
  });

  it("persists and restores an item's tool_calls", async () => {
    const calls: ToolCallDraft[] = [
      draft({ external_call_id: "c1", source_event_id: "e1", session_id: "A" }),
      draft({
        external_call_id: "c2",
        source_event_id: "e1",
        session_id: "A",
        call_index: 1,
        server: "mcp:github",
        name: "create_pr",
        action: {
          surface: "mcp",
          executable: "github",
          param_shape: null,
          command_redacted: "gh pr create --title 'fix'",
          scripts: [],
          extractor: "mcp.generic.v1",
        },
      }),
    ];
    await store.put(item({ source_event_id: "e1", session_id: "A", tool_calls: calls }));

    const bySession = await store.listUnsentBySession();
    const restored = bySession.get("A");
    assert.ok(restored, "session A present");
    assert.equal(restored.length, 1);
    assert.deepEqual(restored[0]?.tool_calls, calls, "tool_calls survive the round-trip");
  });

  it("omits tool_calls entirely for items that carry none", async () => {
    await store.put(item({ source_event_id: "e2", session_id: "B" }));
    const bySession = await store.listUnsentBySession();
    const restored = bySession.get("B");
    assert.ok(restored);
    assert.equal(restored[0] && "tool_calls" in restored[0], false, "key absent, not []");
  });
});
