import assert from "node:assert/strict";
import test from "node:test";
import { Config, LlmCall } from "../index.js";
import { buildBatch, type SeqRef } from "../capture.js";

// `Config` / `LlmCall` are the public API (re-exported from the package root);
// the internal `buildBatch` / `SeqRef` live in capture.js and are imported from
// there to exercise the batching path directly.

const cfg = (): Config =>
  new Config("msk_test", "raw_sdk_openai").withDeviceId("dev_test");

test("buildBatch redacts the excerpt and caps its length", () => {
  const seq: SeqRef = { value: 0 };
  const call = new LlmCall("openai", "sess_1")
    .model("gpt-x")
    .text("here is my key sk-ant-0123456789abcdefghijABCDEF", "ok done");
  const batch = buildBatch(cfg(), [call], seq);

  assert.equal(batch.events.length, 1);
  const ev = batch.events[0]!;
  assert.equal(ev.agent, "raw_sdk_openai");
  assert.equal(ev.provider, "openai");
  assert.equal(ev.model, "gpt-x");
  const excerpt = ev.content_excerpt!;
  assert.ok(excerpt.includes("[REDACTED:anthropic_key]"), excerpt);
  assert.ok(!excerpt.includes("sk-ant-0123"));
  // ≤ 320 code points (+1 for the elision marker if truncated).
  assert.ok(Array.from(excerpt).length <= 320 + 1);
  assert.ok(ev.source_event_id.startsWith("evt_"));
  assert.ok(batch.batch_id.startsWith("batch_"));
  // daemon_version is the producer version key (NOT client_version).
  assert.equal(batch.daemon_version, "node-sdk/0.0.1");
  assert.ok(batch.daemon_version.length <= 40);
});

test("excerpt truncates to exactly 320 code points in the standard path", () => {
  const seq: SeqRef = { value: 0 };
  const long = "x".repeat(500);
  const call = new LlmCall("openai", "sess_1").text(long, "");
  const batch = buildBatch(cfg(), [call], seq);
  const excerpt = batch.events[0]!.content_excerpt!;
  const points = Array.from(excerpt);
  assert.equal(points.length, 321); // 320 + the "…" marker
  assert.equal(points[320], "…");
});

test("empty prompt+completion yields no content_excerpt key", () => {
  const seq: SeqRef = { value: 0 };
  const call = new LlmCall("openai", "sess_1");
  const batch = buildBatch(cfg(), [call], seq);
  assert.ok(!("content_excerpt" in batch.events[0]!));
});

test("optional keys are omitted entirely when absent (never null)", () => {
  const seq: SeqRef = { value: 0 };
  const call = new LlmCall("openai", "sess_1"); // no model/cwd/git/duration/pricing_mode
  const batch = buildBatch(cfg(), [call], seq);
  const ev = batch.events[0]!;
  for (const k of ["model", "cwd", "git", "duration_ms", "pricing_mode"]) {
    assert.ok(!(k in ev), `expected ${k} omitted, present in ${JSON.stringify(ev)}`);
  }
  // tool_calls omitted at the batch level when empty.
  assert.ok(!("tool_calls" in batch));
  // The all-zero tokens object is still present.
  assert.deepEqual(ev.tokens, {
    input: 0,
    output: 0,
    cache_creation: 0,
    cache_read: 0,
    reasoning: 0,
  });
});

test("tool calls carry hashes/sizes, never raw args", () => {
  const seq: SeqRef = { value: 0 };
  const call = new LlmCall("anthropic", "sess_1");
  const args = { command: "rm -rf /tmp/secret", timeout: 5 };
  call.toolCalls.push({
    name: "Bash",
    server: "builtin",
    args,
    resultBytes: 128,
    status: "success",
    commandFamilies: ["rm"],
  });
  const batch = buildBatch(cfg(), [call], seq);

  assert.equal(batch.tool_calls!.length, 1);
  const tc = batch.tool_calls![0]!;
  assert.equal(tc.name, "Bash");
  assert.equal(tc.server, "builtin");
  assert.equal(tc.call_index, 0);
  assert.equal(tc.result_bytes, 128);
  assert.deepEqual(tc.command_families, ["rm"]);
  // args_bytes == byte length of the compact JSON.
  assert.equal(tc.args_bytes, Buffer.byteLength(JSON.stringify(args)));
  assert.equal(tc.args_hash.length, 64); // sha256 hex
  assert.notEqual(tc.signature_hash, "none");
  assert.ok(tc.external_call_id.startsWith("tc_"));
  assert.equal(tc.external_call_id.length, "tc_".length + 16);

  // The raw command must never appear anywhere in the serialized batch.
  const json = JSON.stringify(batch);
  assert.ok(!json.includes("rm -rf /tmp/secret"), "raw args leaked into wire");
});

test("signature_hash is 'none' for non-object args; args_hash empty for no args", () => {
  const seq: SeqRef = { value: 0 };
  const call = new LlmCall("anthropic", "sess_1");
  call.toolCalls.push({ name: "NoArgs", status: "success" }); // args undefined
  call.toolCalls.push({ name: "ArrayArgs", args: [1, 2, 3], status: "success" });
  const batch = buildBatch(cfg(), [call], seq);

  const [noArgs, arrArgs] = batch.tool_calls!;
  assert.equal(noArgs!.args_hash, "");
  assert.equal(noArgs!.signature_hash, "none");
  assert.equal(noArgs!.args_bytes, 0);
  // array => not a plain object => signature "none", but args still hashed.
  assert.equal(arrArgs!.signature_hash, "none");
  assert.equal(arrArgs!.args_hash.length, 64);
});

test("command_families cap at 3 and omit when empty", () => {
  const seq: SeqRef = { value: 0 };
  const call = new LlmCall("anthropic", "sess_1");
  call.toolCalls.push({
    name: "Multi",
    status: "success",
    commandFamilies: ["a", "b", "c", "d", "e"],
  });
  call.toolCalls.push({ name: "None", status: "success" });
  const batch = buildBatch(cfg(), [call], seq);

  assert.deepEqual(batch.tool_calls![0]!.command_families, ["a", "b", "c"]);
  assert.ok(!("command_families" in batch.tool_calls![1]!));
});

test("raw mode sends full untruncated turns, still floor-redacted", () => {
  const seq: SeqRef = { value: 0 };
  const remoteCfg = new Config("msk", "raw_sdk_openai")
    .withDeviceId("dev_test")
    .withRemote("https://api.modelstat.ai", true);
  const long = "word ".repeat(200); // > 320 chars
  const call = new LlmCall("openai", "sess_1").text(long, "AKIAIOSFODNN7EXAMPLE");
  const batch = buildBatch(remoteCfg, [call], seq);
  const excerpt = batch.events[0]!.content_excerpt!;
  assert.ok(Array.from(excerpt).length > 320, "raw mode must not truncate");
  assert.ok(
    excerpt.includes("[REDACTED:aws_access_key]"),
    "floor still applies in raw mode",
  );
});

test("redaction policy 'none' leaves text raw (still capped in standard path)", () => {
  const seq: SeqRef = { value: 0 };
  const c = cfg();
  c.redaction = "none";
  const call = new LlmCall("openai", "sess_1").text("email me jane@example.com", "");
  const batch = buildBatch(c, [call], seq);
  const excerpt = batch.events[0]!.content_excerpt!;
  assert.ok(excerpt.includes("jane@example.com"), excerpt);
});

test("ts and started_at are RFC3339 UTC with millisecond precision", () => {
  const seq: SeqRef = { value: 0 };
  const call = new LlmCall("openai", "sess_1");
  call.startedAt = new Date("2026-06-19T00:00:00.000Z");
  call.toolCalls.push({ name: "T", status: "success" });
  const batch = buildBatch(cfg(), [call], seq);
  assert.equal(batch.events[0]!.ts, "2026-06-19T00:00:00.000Z");
  assert.equal(batch.tool_calls![0]!.started_at, "2026-06-19T00:00:00.000Z");
});
