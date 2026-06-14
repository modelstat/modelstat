import { strict as assert } from "node:assert";
import { createHash } from "node:crypto";
import { test } from "node:test";
import {
  fallbackCallId,
  hashArgs,
  jsonBytes,
  normalizeToolName,
  SIGNATURE_NONE,
  splitObservedToolName,
  toolIdentity,
} from "./index.js";

const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

test("hashArgs: object input hashes JSON + sorted key signature", () => {
  const input = { file_path: "/tmp/x", limit: 5 };
  const out = hashArgs(input);
  assert.equal(out.args_hash, sha256(JSON.stringify(input)), "args_hash = sha256 of JSON");
  assert.equal(out.signature_hash, sha256("file_path,limit"), "signature = sorted keys");
  assert.equal(out.args_bytes, Buffer.byteLength(JSON.stringify(input), "utf8"));
});

test("hashArgs: key order does not change signature_hash", () => {
  const a = hashArgs({ b: 1, a: 2 });
  const b = hashArgs({ a: 2, b: 1 });
  assert.equal(a.signature_hash, b.signature_hash, "signature is key-order independent");
});

test("hashArgs: no input → empty hash, 'none' signature, 0 bytes", () => {
  for (const input of [undefined, null]) {
    const out = hashArgs(input);
    assert.equal(out.args_hash, "");
    assert.equal(out.signature_hash, SIGNATURE_NONE);
    assert.equal(out.args_bytes, 0);
  }
});

test("hashArgs: non-object / empty-object input → 'none' signature, real hash", () => {
  for (const input of ["a string", 42, true, [1, 2], {}]) {
    const out = hashArgs(input);
    assert.equal(out.signature_hash, SIGNATURE_NONE, `signature for ${JSON.stringify(input)}`);
    assert.equal(out.args_hash, sha256(JSON.stringify(input)));
    assert.ok(out.args_bytes > 0);
  }
});

test("hashArgs: args_bytes counts UTF-8 bytes, not chars", () => {
  const out = hashArgs({ s: "héllo" });
  assert.equal(out.args_bytes, Buffer.byteLength(JSON.stringify({ s: "héllo" }), "utf8"));
});

test("jsonBytes: unmatched/empty results count 0", () => {
  assert.equal(jsonBytes(undefined), 0);
  assert.equal(jsonBytes(null), 0);
  assert.equal(jsonBytes(""), 0);
  assert.equal(jsonBytes("ok"), Buffer.byteLength('"ok"', "utf8"));
  assert.equal(
    jsonBytes([{ type: "text", text: "hi" }]),
    Buffer.byteLength('[{"type":"text","text":"hi"}]', "utf8"),
  );
});

test("normalizeToolName: trims, NFC-normalises, caps at 120", () => {
  assert.equal(normalizeToolName("  Bash  "), "Bash");
  // e + combining acute (NFD) folds into precomposed é (NFC)
  assert.equal(normalizeToolName("cafe\u0301"), "caf\u00e9", "NFD input folds to NFC");
  assert.equal(normalizeToolName("x".repeat(300)).length, 120);
});

test("normalizeToolName: hex/UUID-ish tails of ≥8 chars become <dyn>", () => {
  assert.equal(normalizeToolName("subscribe_a1b2c3d4e5f6"), "subscribe_<dyn>");
  assert.equal(
    normalizeToolName("job-550e8400-e29b-41d4-a716-446655440000"),
    "job-<dyn>",
    "full UUID collapses to one <dyn>",
  );
  assert.equal(normalizeToolName("Bash"), "Bash", "short names untouched");
  assert.equal(normalizeToolName("create_pr"), "create_pr", "non-hex segments untouched");
});

test("splitObservedToolName: mcp__server__tool → mcp:server / tool", () => {
  assert.deepEqual(splitObservedToolName("mcp__github__create_pr"), {
    server: "mcp:github",
    name: "create_pr",
  });
  assert.deepEqual(splitObservedToolName("mcp__brave-search__web_search"), {
    server: "mcp:brave-search",
    name: "web_search",
  });
  assert.deepEqual(
    splitObservedToolName("mcp__my_server__tool"),
    { server: "mcp:my_server", name: "tool" },
    "underscored server names use the shortest-server split",
  );
});

test("splitObservedToolName: everything else is builtin", () => {
  assert.deepEqual(splitObservedToolName("Bash"), { server: "builtin", name: "Bash" });
  assert.deepEqual(splitObservedToolName("WebSearch"), { server: "builtin", name: "WebSearch" });
});

test("toolIdentity: builtin = bare name, MCP = server/name", () => {
  assert.equal(toolIdentity("builtin", "Bash"), "Bash");
  assert.equal(toolIdentity("mcp:github", "create_pr"), "mcp:github/create_pr");
});

test("fallbackCallId: deterministic tc_ id, varies with inputs", () => {
  const a = fallbackCallId("evt_abc", 0);
  assert.equal(a, fallbackCallId("evt_abc", 0), "same inputs → same id");
  assert.ok(a.startsWith("tc_"));
  assert.notEqual(a, fallbackCallId("evt_abc", 1));
  assert.notEqual(a, fallbackCallId("evt_def", 0));
});
