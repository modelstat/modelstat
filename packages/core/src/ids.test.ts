import { strict as assert } from "node:assert";
import { test } from "node:test";
import { fallbackCallId, paramShape, sourceEventId } from "./ids.js";

// The server dedupes on (scope_id, source_event_id), so the derivation is a
// wire contract: if these golden values change, every previously-uploaded
// event re-ingests under a new id and history double-counts.
test("fs-shape derivation is stable (wire contract)", () => {
  assert.equal(sourceEventId("dev_1", { file: "/x/a.jsonl", byteOffset: 42 }), "evt_16zw770jnvito");
});

test("lineUuid shape ignores file position and is stable (wire contract)", () => {
  const uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
  assert.equal(sourceEventId("dev_1", { lineUuid: uuid }), "evt_irnlblnsf9gx");
  // Same line copied into another file/offset → same key; that's the
  // whole point (resume-copy dedupe).
  assert.notEqual(
    sourceEventId("dev_1", { lineUuid: uuid }),
    sourceEventId("dev_1", { file: "/x/a.jsonl", byteOffset: 42 }),
  );
  assert.notEqual(
    sourceEventId("dev_1", { lineUuid: uuid }),
    sourceEventId("dev_2", { lineUuid: uuid }),
    "device id still partitions the key space",
  );
});

// param_shape is value-masked and tier-1; the daemon derives it on-device
// from raw args (which never ship), so a shared reference impl is the only
// cross-repo determinism check. These MUST match the backend's Rust
// implementation byte-for-byte. `§` = U+00A7.
test("paramShape keeps flags, masks every value (cross-repo golden vectors)", () => {
  assert.equal(paramShape("rollout restart deploy/payments-api -n prod"), "§ § § -n §");
  assert.equal(paramShape('commit -m "fix bug"'), "§ -m § §");
  assert.equal(paramShape("-la /etc/passwd"), "-la §");
  assert.equal(paramShape("install react"), "§ §");
  assert.equal(paramShape("--namespace=prod get pods"), "--namespace=§ § §");
  assert.equal(paramShape("run --watch -j4 test/unit"), "§ --watch -j4 §");
  assert.equal(paramShape("status"), "§");
  assert.equal(paramShape(""), "");
  assert.equal(paramShape("   "), "");
});

// tc_ ids are the tool-call dedupe key for every source line that carries no
// id of its own, so the derivation is a wire contract exactly like evt_ above.
test("fallbackCallId: deterministic tc_ id, varies with both inputs", () => {
  const a = fallbackCallId("evt_abc", 0);
  assert.equal(a, fallbackCallId("evt_abc", 0), "same inputs -> same id");
  assert.ok(a.startsWith("tc_"));
  assert.notEqual(a, fallbackCallId("evt_abc", 1), "call index partitions");
  assert.notEqual(a, fallbackCallId("evt_def", 0), "event id partitions");
});
