import { strict as assert } from "node:assert";
import { test } from "node:test";
import {
  applyLlmRedactions,
  composeRedactors,
  LLM_REDACTION_MARKER,
  parseRedactReply,
  shouldDeepRedact,
} from "./redaction.js";

test("shouldDeepRedact gates on plausible-secret signals only", () => {
  // worth a model call
  assert.equal(shouldDeepRedact('CK="xk_edge_examplefake0000key0000"'), true);
  assert.equal(shouldDeepRedact("curl -H 'Authorization: Bearer abc'"), true);
  assert.equal(shouldDeepRedact("psql postgres://u:p@host/db"), true);
  assert.equal(shouldDeepRedact("deploy --token abcdefghijklmnopqrst"), true);
  // not worth a model call (the common case)
  assert.equal(shouldDeepRedact("git status"), false);
  assert.equal(shouldDeepRedact("ls -la"), false); // a 2-char flag, no long token
  assert.equal(shouldDeepRedact("cat README"), false);
  assert.equal(shouldDeepRedact(""), false);
});

test("parseRedactReply keeps long candidates, drops noise", () => {
  const reply = `<think>looking</think>
xk_edge_examplefake0000key0000
"examplefake0000tgauth0000"
prod
NONE
[REDACTED:hash]
secret
xk_edge_examplefake0000key0000`;
  // Note: stripThinking happens in the llama adapter; parseRedactReply sees the
  // post-strip text. Simulate that the <think> line is just ignored as noise.
  const got = parseRedactReply(reply);
  assert.ok(got.includes("xk_edge_examplefake0000key0000"));
  assert.ok(got.includes("examplefake0000tgauth0000")); // quotes stripped
  assert.ok(!got.includes("prod"), "too short / safe word dropped");
  assert.ok(!got.includes("secret"), "safe word dropped");
  assert.ok(!got.some((s) => s.startsWith("[REDACTED")), "existing markers dropped");
  assert.equal(got.filter((s) => s === "xk_edge_examplefake0000key0000").length, 1, "deduped");
});

test("applyLlmRedactions replaces only verbatim occurrences, longest-first", () => {
  const text = 'CK="sk_live_abcdefghijklmnop"; echo sk_live_abcdefghijklmnop';
  const r = applyLlmRedactions(text, ["sk_live_abcdefghijklmnop"]);
  assert.ok(!r.text.includes("sk_live_abcdefghijklmnop"), "all occurrences replaced");
  assert.equal((r.text.match(/\[REDACTED:llm\]/g) ?? []).length, 2);
  assert.equal(r.count, 1);

  // A hallucinated candidate not present in the text is a no-op.
  const r2 = applyLlmRedactions("git status", ["not-in-the-text-at-all"]);
  assert.equal(r2.text, "git status");
  assert.equal(r2.count, 0);

  // Overlapping candidates: the longer wins, no partial mangling.
  const r3 = applyLlmRedactions("token=abcdefgh-suffix", ["abcdefgh", "abcdefgh-suffix"]);
  assert.equal(r3.text, `token=${LLM_REDACTION_MARKER}`);
});

test("composeRedactors chains layers, merges counts, and is fail-safe", async () => {
  const layer2 = async (t: string) => ({
    text: t.replace("PII", "[REDACTED:NAME]"),
    counts: { pf_name: 1 },
  });
  const thrower = async (_t: string) => {
    throw new Error("model down");
  };
  const layer3 = async (t: string) => ({
    text: t.replace("SECRET", "[REDACTED:llm]"),
    counts: { llm_secrets: 1 },
  });

  const chain = composeRedactors(layer2, thrower, layer3);
  const r = await chain("PII and SECRET here");
  // layer2 + layer3 applied; the thrower is skipped, not fatal.
  assert.equal(r.text, "[REDACTED:NAME] and [REDACTED:llm] here");
  assert.deepEqual(r.counts, { pf_name: 1, llm_secrets: 1 });
});
