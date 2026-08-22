import assert from "node:assert/strict";
import test from "node:test";
import { Config, LlmCall, withMetadata, capMetadata } from "../index.js";
import { buildBatch, type SeqRef } from "../capture.js";

const cfg = (): Config =>
  new Config("msk_test", "raw_sdk_openai", "test-app").withDeviceId("dev_test");

test("no metadata anywhere → key omitted on the wire", () => {
  const seq: SeqRef = { value: 0 };
  const batch = buildBatch(cfg(), [new LlmCall("openai", "sess_1")], seq);
  assert.ok(!("metadata" in batch.events[0]!));
  assert.ok(!JSON.stringify(batch).includes('"metadata"'));
});

test("precedence: Config defaults < per-call (per-call wins on shared key)", () => {
  const seq: SeqRef = { value: 0 };
  const c = cfg();
  c.metadata = { environment: "prod", feature: "default_feature" };
  const call = new LlmCall("openai", "sess_1").metadata({
    feature: "search",
    team: "growth",
  });
  const batch = buildBatch(c, [call], seq);
  const md = batch.events[0]!.metadata!;
  assert.equal(md.environment, "prod"); // default-only survives
  assert.equal(md.feature, "search"); // per-call overrides default
  assert.equal(md.team, "growth"); // per-call-only added
  // Serializes as a flat object.
  assert.equal(
    JSON.parse(JSON.stringify(batch)).events[0].metadata.feature,
    "search",
  );
});

test("precedence: Config < ambient < per-call (each later layer wins)", async () => {
  // The ambient layer is captured on the hot path at record() time, so this
  // must run through the real Client/worker, not buildBatch directly.
  const { Client, FakeTransport } = await import("../index.js");
  const c = cfg();
  c.metadata = { a: "config", b: "config", c: "config" };
  const fake = new FakeTransport();
  const ms = Client.withTransport(c, fake);

  withMetadata({ b: "ambient", c: "ambient" }, () => {
    // per-call overrides only `c`.
    ms.record(new LlmCall("openai", "sess_1").metadata({ c: "percall" }));
  });
  await ms.flush();

  const md = fake.batches()[0]!.events[0]!.metadata!;
  assert.equal(md.a, "config"); // config-only
  assert.equal(md.b, "ambient"); // ambient overrides config
  assert.equal(md.c, "percall"); // per-call overrides ambient + config
  await ms.shutdown();
});

test("ambient scope auto-resets on exit (and on throw)", async () => {
  const { Client, FakeTransport } = await import("../index.js");
  const fake = new FakeTransport();
  const ms = Client.withTransport(cfg(), fake);

  withMetadata({ scoped: "yes" }, () => {
    ms.record(new LlmCall("openai", "inside"));
  });
  // Outside the scope: no ambient tag.
  ms.record(new LlmCall("openai", "outside"));

  // And a throwing scope still resets.
  assert.throws(() => {
    withMetadata({ scoped: "boom" }, () => {
      throw new Error("boom");
    });
  });
  ms.record(new LlmCall("openai", "after-throw"));

  await ms.flush();
  const events = fake.batches().flatMap((b) => b.events);
  const inside = events.find((e) => e.session_id === "inside")!;
  const outside = events.find((e) => e.session_id === "outside")!;
  const afterThrow = events.find((e) => e.session_id === "after-throw")!;
  assert.equal(inside.metadata!.scoped, "yes");
  assert.ok(!("metadata" in outside));
  assert.ok(!("metadata" in afterThrow));
  await ms.shutdown();
});

test("caps: excess keys dropped deterministically by sorted key order", () => {
  const seq: SeqRef = { value: 0 };
  const c = cfg();
  // 20 keys k00..k19 → only the 16 smallest survive (k00..k15).
  for (let i = 0; i < 20; i++) {
    c.metadata[`k${String(i).padStart(2, "0")}`] = "v";
  }
  const batch = buildBatch(c, [new LlmCall("openai", "sess_1")], seq);
  const md = batch.events[0]!.metadata!;
  assert.equal(Object.keys(md).length, 16);
  assert.ok("k15" in md);
  assert.ok(!("k16" in md));
});

test("caps: over-long key and value are truncated (no elision marker)", () => {
  const out = capMetadata({ ["k".repeat(100)]: "v".repeat(500) });
  const [key, value] = Object.entries(out)[0]!;
  assert.equal(Array.from(key).length, 64);
  assert.equal(Array.from(value).length, 256);
  assert.ok(!value.endsWith("…"));
});

test("metadata serializes when present and omits when empty", () => {
  const seq: SeqRef = { value: 0 };
  // Present.
  const call = new LlmCall("openai", "sess_1").metadata({ feature: "x" });
  const batch = buildBatch(cfg(), [call], seq);
  assert.deepEqual(batch.events[0]!.metadata, { feature: "x" });

  // Empty per-call + empty config → omitted.
  const empty = buildBatch(cfg(), [new LlmCall("openai", "sess_2")], {
    value: 0,
  });
  assert.ok(!("metadata" in empty.events[0]!));
});
