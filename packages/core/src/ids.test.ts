import { strict as assert } from "node:assert";
import { test } from "node:test";
import { sourceEventId } from "./ids.js";

// The server dedupes on (scope_id, source_event_id), so the derivation is a
// wire contract: if these golden values change, every previously-uploaded
// event re-ingests under a new id and history double-counts.
test("fs-shape derivation is stable (wire contract)", () => {
  assert.equal(sourceEventId("dev_1", "/x/a.jsonl", 42), "evt_16zw770jnvito");
  // Legacy 3-arg form and the object form hash identically.
  assert.equal(
    sourceEventId("dev_1", "/x/a.jsonl", 42),
    sourceEventId("dev_1", { file: "/x/a.jsonl", byteOffset: 42 }),
  );
});

test("lineUuid shape ignores file position and is stable (wire contract)", () => {
  const uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
  assert.equal(sourceEventId("dev_1", { lineUuid: uuid }), "evt_irnlblnsf9gx");
  // Same line copied into another file/offset → same key; that's the
  // whole point (resume-copy dedupe).
  assert.notEqual(
    sourceEventId("dev_1", { lineUuid: uuid }),
    sourceEventId("dev_1", "/x/a.jsonl", 42),
  );
  assert.notEqual(
    sourceEventId("dev_1", { lineUuid: uuid }),
    sourceEventId("dev_2", { lineUuid: uuid }),
    "device id still partitions the key space",
  );
});
