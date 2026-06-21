/**
 * The dependency-free extractive fallback summariser — the always-works path
 * when the bundled LLM can't load. Must always produce real, non-empty,
 * non-placeholder output from the same inputs the LLM path gets.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { ABSTRACT_OUTPUT_MAX_CHARS } from "./prompts.js";
import { heuristicSummarize } from "./heuristic-summary.js";

const summarize = heuristicSummarize();

test("leads with the intent from structured excerpts and appends facts", async () => {
  const out = await summarize({
    prompt: "",
    maxTokens: 120,
    excerpts: ["fix the daemon Metal crash so re-ingest works", "tried the CPU fallback"],
    facts: "repo modelstat/public; 12 turns on claude_code; files touched: llama.ts",
  });
  assert.match(out, /Fix the daemon Metal crash/);
  assert.match(out, /repo modelstat\/public/);
  // Not the rejected metadata placeholder shape ("N turns on agent" ALONE).
  assert.ok(!/^\d+ turns on \w+$/.test(out));
});

test("skips a greeting and picks the substantive line", async () => {
  const out = await summarize({
    prompt: "",
    maxTokens: 120,
    excerpts: ["hi there", "please add OAuth login to the settings page"],
    facts: "",
  });
  assert.match(out, /Add OAuth login to the settings page/);
  assert.doesNotMatch(out, /^Hi there/);
});

test("strips leading politeness/filler so the lead reads as an action", async () => {
  const out = await summarize({
    prompt: "",
    maxTokens: 120,
    excerpts: ["can you refactor the auth middleware to use the new token store"],
    facts: "",
  });
  assert.match(out, /^Refactor the auth middleware/);
});

test("falls back to parsing excerpts out of the prompt when none are passed", async () => {
  const prompt =
    'Session context: repo acme/api; 5 turns on codex.\n\nSampled excerpts:\n  [turn 1] "debug the failing CI pipeline for the release job"\n  [turn 2] "it was a missing env var"\n\nWrite ONE sentence.';
  const out = await summarize({ prompt, maxTokens: 120 });
  assert.match(out, /debug the failing CI pipeline/i);
  assert.match(out, /repo acme\/api/);
});

test("never returns empty, even with a single useless excerpt", async () => {
  const out = await summarize({ prompt: "", maxTokens: 120, excerpts: ["ok"], facts: "" });
  assert.ok(out.trim().length > 0);
});

test("clamps to the abstract length cap on a long excerpt", async () => {
  const long = `implement ${"a very detailed multi-part feature ".repeat(40)}`;
  const out = await summarize({ prompt: "", maxTokens: 120, excerpts: [long], facts: "" });
  assert.ok(out.length <= ABSTRACT_OUTPUT_MAX_CHARS, `length ${out.length} <= ${ABSTRACT_OUTPUT_MAX_CHARS}`);
});
