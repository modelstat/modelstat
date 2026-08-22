import assert from "node:assert/strict";
import test from "node:test";
import {
  Client,
  Config,
  endpoint,
  FakeTransport,
  LlmCall,
  type IngestBatch,
} from "../index.js";

test("record then flush delivers a redacted batch", async () => {
  const cfg = new Config("msk_test", "raw_sdk_openai", "test-app").withDeviceId("dev_test");
  const fake = new FakeTransport();
  const ms = Client.withTransport(cfg, fake);

  ms.record(
    new LlmCall("openai", "sess_1")
      .model("gpt-x")
      .tokens({ input: 100, output: 20 })
      .text("my email is jane@example.com", "done"),
  );
  await ms.flush();

  const batches = fake.batches();
  assert.equal(batches.length, 1);
  const ev = batches[0]!.events[0]!;
  assert.equal(ev.provider, "openai");
  assert.equal(ev.tokens.input, 100);
  assert.equal(ev.tokens.output, 20);
  const excerpt = ev.content_excerpt!;
  assert.ok(excerpt.includes("[REDACTED:email]"), `got ${excerpt}`);
  assert.ok(!excerpt.includes("jane@example.com"));
  assert.equal(ms.dropped(), 0);

  await ms.shutdown();
});

test("overflow drops the newest and counts without blocking", async () => {
  // Tiny buffer, and never flush, so the buffer fills.
  const cfg = new Config("msk", "raw_sdk_generic", "test-app").withDeviceId("dev_test");
  cfg.bufferCapacity = 2;
  cfg.flushIntervalMs = 3_600_000; // effectively never
  const fake = new FakeTransport();
  const ms = Client.withTransport(cfg, fake);

  for (let i = 0; i < 50; i++) {
    ms.record(new LlmCall("openai", "sess_1"));
  }
  // record() never blocked and overflow was counted.
  assert.ok(ms.dropped() > 0, `expected some drops, got ${ms.dropped()}`);

  await ms.shutdown();
});

test("a full batch flushes eagerly without an explicit flush", async () => {
  const cfg = new Config("msk", "raw_sdk_openai", "test-app").withDeviceId("dev_test");
  cfg.flushMaxBatch = 3;
  cfg.flushIntervalMs = 3_600_000; // rely on the size trigger, not the timer
  const fake = new FakeTransport();
  const ms = Client.withTransport(cfg, fake);

  for (let i = 0; i < 3; i++) {
    ms.record(new LlmCall("openai", `sess_${i}`));
  }
  // Let the eager flush's microtasks settle.
  await ms.flush();

  const batches = fake.batches();
  const totalEvents = batches.reduce(
    (n: number, b: IngestBatch) => n + b.events.length,
    0,
  );
  assert.equal(totalEvents, 3);
  assert.equal(ms.dropped(), 0);

  await ms.shutdown();
});

test("the periodic timer flushes buffered calls", async () => {
  const cfg = new Config("msk", "raw_sdk_openai", "test-app").withDeviceId("dev_test");
  cfg.flushIntervalMs = 20; // fast tick for the test
  const fake = new FakeTransport();
  const ms = Client.withTransport(cfg, fake);

  ms.record(new LlmCall("openai", "sess_1"));
  // Wait long enough for at least one timer tick + its async flush.
  await new Promise((r) => setTimeout(r, 80));

  assert.ok(fake.batches().length >= 1, "timer should have flushed");
  await ms.shutdown();
});

test("flush on an empty buffer is a no-op (no batch sent)", async () => {
  const cfg = new Config("msk", "raw_sdk_openai", "test-app");
  const fake = new FakeTransport();
  const ms = Client.withTransport(cfg, fake);
  await ms.flush();
  assert.equal(fake.batches().length, 0);
  await ms.shutdown();
});

test("endpoint resolution per mode", () => {
  // Default: local daemon loopback.
  const local = new Config("k", "a", "test-app");
  assert.equal(endpoint(local.mode), "http://127.0.0.1:4319/v1/ingest");

  // Remote, non-raw: /v1/ingest with a trimmed trailing slash.
  const remote = new Config("k", "a", "test-app").withRemote("https://api.modelstat.ai/", false);
  assert.equal(endpoint(remote.mode), "https://api.modelstat.ai/v1/ingest");
  assert.equal(remote.sendsFullTurns(), false);

  // Remote, raw: /v1/ingest/raw.
  const raw = new Config("k", "a", "test-app").withRemote("https://api.modelstat.ai", true);
  assert.equal(endpoint(raw.mode), "https://api.modelstat.ai/v1/ingest/raw");
  assert.equal(raw.sendsFullTurns(), true);
});

test("worker retries once on transient failure then succeeds", async () => {
  // A transport that throws on the first call, succeeds on the second.
  let calls = 0;
  const flaky = {
    sent: undefined as IngestBatch | undefined,
    send(batch: IngestBatch): Promise<void> {
      calls += 1;
      if (calls === 1) {
        return Promise.reject(new Error("boom"));
      }
      this.sent = batch;
      return Promise.resolve();
    },
  };
  const cfg = new Config("msk", "raw_sdk_openai", "test-app");
  const ms = Client.withTransport(cfg, flaky);
  ms.record(new LlmCall("openai", "sess_1"));
  await ms.flush(); // includes the 250ms retry backoff

  assert.equal(calls, 2, "should retry exactly once");
  assert.ok(flaky.sent !== undefined, "retry should deliver the batch");
  await ms.shutdown();
});
