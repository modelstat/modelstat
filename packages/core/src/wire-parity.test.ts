/**
 * TS↔Rust wire parity (plan D16, feature §4.7).
 *
 * The golden wire fixtures under daemon/crates/modelstat-wire/tests/golden/wire
 * are the shared contract between this TS schema and the Rust `modelstat-wire`
 * port. This test proves the two directions:
 *
 *   1. The TS-produced fixtures (`wire/*.json`) still Zod-parse — guards against
 *      a hand-edit or a TS schema change that would desync the committed golden.
 *   2. The RUST-emitted fixtures (`wire/rust-emitted/*.json`, written by
 *      `cargo run -p modelstat-wire --example emit_fixtures`) Zod-parse — i.e.
 *      **TS accepts Rust's serialization of the wire**.
 *
 * The mirror direction (Rust accepts TS wire) is asserted in
 * daemon/crates/modelstat-wire/tests/golden_wire.rs. Together they close D16.
 */
import { strict as assert } from "node:assert";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import type { ZodTypeAny } from "zod";
import { HeartbeatPayload, IngestBatch, RawEvent, Segment, ToolCallWire } from "./schemas.js";

const WIRE_DIR = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
  "daemon",
  "crates",
  "modelstat-wire",
  "tests",
  "golden",
  "wire",
);

/** fixture file → the schema that must accept it. */
const SCHEMA_BY_FILE: Record<string, ZodTypeAny> = {
  "raw_event_full.json": RawEvent,
  "raw_event_minimal.json": RawEvent,
  "raw_event_sdk_instants.json": RawEvent,
  "tool_call.json": ToolCallWire,
  "segment.json": Segment,
  "segment_with_embedding.json": Segment,
  "ingest_batch.json": IngestBatch,
  "heartbeat.json": HeartbeatPayload,
};

function parseFixture(dir: string, file: string): unknown {
  const schema = SCHEMA_BY_FILE[file]!;
  const raw = JSON.parse(readFileSync(join(dir, file), "utf8"));
  // `.parse` throws on any schema violation — that IS the assertion.
  return schema.parse(raw);
}

test("TS schemas accept the TS-produced golden wire fixtures", () => {
  for (const file of Object.keys(SCHEMA_BY_FILE)) {
    assert.doesNotThrow(() => parseFixture(WIRE_DIR, file), `TS golden ${file}`);
  }
});

test("TS schemas accept the Rust-emitted wire fixtures (TS accepts Rust wire)", () => {
  const rustDir = join(WIRE_DIR, "rust-emitted");
  for (const file of Object.keys(SCHEMA_BY_FILE)) {
    assert.doesNotThrow(() => parseFixture(rustDir, file), `rust-emitted ${file}`);
  }
  // Spot-check that data survived the Rust round-trip, not just the shape.
  const batch = IngestBatch.parse(
    JSON.parse(readFileSync(join(rustDir, "ingest_batch.json"), "utf8")),
  );
  assert.equal(batch.events.length, 3);
  assert.equal(batch.tool_calls.length, 1);
  assert.equal(batch.summarizer_mode, "cloud");
  // The additive facts of this wave survive Rust's serialization: the source-log
  // ordinal, the SDK's own call instants, and the device's zone — offset AND
  // IANA name, in the contract pair the server reads.
  assert.equal(batch.tz, "America/Los_Angeles");
  assert.equal(batch.tz_offset_minutes, -420);
  assert.equal(batch.events[0]!.seq, 128);
  const sdk = batch.events[2]!;
  assert.equal(sdk.started_at, "2026-06-01T10:00:00.000Z");
  assert.equal(sdk.first_token_at, "2026-06-01T10:00:00.140Z");
  const hb = HeartbeatPayload.parse(
    JSON.parse(readFileSync(join(rustDir, "heartbeat.json"), "utf8")),
  );
  assert.equal(hb.timezone, "America/Los_Angeles");
  assert.equal(hb.utc_offset_minutes, -420);
});

test("tool input uses the full-message UTF-8 byte guard", () => {
  const fixture = JSON.parse(readFileSync(join(WIRE_DIR, "tool_call.json"), "utf8"));
  fixture.action.input_redacted = "x".repeat(24_173);
  assert.doesNotThrow(() => ToolCallWire.parse(fixture));

  fixture.action.input_redacted = "€".repeat(100_000);
  assert.throws(() => ToolCallWire.parse(fixture));

  fixture.action.input_redacted = "safe";
  fixture.action.command_redacted = "x".repeat(24_173);
  assert.doesNotThrow(() => ToolCallWire.parse(fixture));
  fixture.action.command_redacted = "€".repeat(100_000);
  assert.throws(() => ToolCallWire.parse(fixture));
});

test("tool input evidence rejects invalid pairs and formats", () => {
  const fixture = JSON.parse(readFileSync(join(WIRE_DIR, "tool_call.json"), "utf8"));
  for (const [input, format] of [
    ["code", null],
    [null, "text"],
    ["code", "yaml"],
  ]) {
    fixture.action.input_redacted = input;
    fixture.action.input_format = format;
    assert.throws(() => ToolCallWire.parse(fixture));
  }
});
