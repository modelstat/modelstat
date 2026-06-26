/**
 * Regression tests for the privacy-filter's optional-dep loading —
 * same contract as the embedder (see ../node/transformersjs-embed.test.ts):
 * a missing @huggingface/transformers must latch the loader off after
 * one import attempt and one warning, with redaction degrading to
 * pass-through, no matter how many segments a scan processes.
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import { OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS } from "../optional-module.js";
import { createPrivacyFilterRedactor, reconstructSurface } from "./privacy-filter.js";

/** The fields the redactor reads off a transformers.js token. `start`/`end`
 * are optional: the redactor uses them when present (precise slicing) and
 * falls back to surface reconstruction when they're absent. */
type FakeToken = { entity: string; word: string; start?: number; end?: number };

/** A fake @huggingface/transformers whose token-classifier returns a fixed
 * token list, so we exercise the REAL redaction logic against the actual
 * wire shape transformers.js produces (notably: no start/end offsets). */
function fakeTransformers(tokens: FakeToken[]): { importModule: (id: string) => Promise<unknown> } {
  return {
    importModule: async () => ({
      pipeline: async () => async (_text: string) => tokens,
    }),
  };
}

function missingPackageError(): Error {
  const err = new Error(
    "Cannot find package '@huggingface/transformers' imported from /Users/x/.modelstat/bin/modelstat.mjs",
  );
  (err as Error & { code?: string }).code = "ERR_MODULE_NOT_FOUND";
  return err;
}

function captureWarns(): { warns: unknown[][]; restore: () => void } {
  const warns: unknown[][] = [];
  const orig = console.warn;
  console.warn = (...args: unknown[]) => {
    warns.push(args);
  };
  return { warns, restore: () => (console.warn = orig) };
}

test("missing package: one import attempt + one warn, pass-through redaction", async () => {
  let importCalls = 0;
  const { warns, restore } = captureWarns();
  try {
    const redactor = await createPrivacyFilterRedactor({
      importModule: async () => {
        importCalls += 1;
        throw missingPackageError();
      },
    });
    for (let i = 0; i < 500; i++) {
      const input = `segment abstract ${i}`;
      const out = await redactor(input);
      assert.equal(out.text, input);
    }
    assert.equal(importCalls, 1, "failing import must not run per segment");
    assert.equal(warns.length, 1, "unavailability must be warned exactly once");
  } finally {
    restore();
  }
});

test("redacts offset-LESS BIO tokens (the real transformers.js bert-base-NER shape)", async () => {
  // Regression: transformers.js 3.x omits start/end for this model, so the
  // old offset-only logic skipped every entity and redacted NOTHING.
  const text = "Escalate the incident to Katherine Johnson at Globex Corporation.";
  const redactor = await createPrivacyFilterRedactor(
    fakeTransformers([
      { entity: "B-PER", word: "Katherine" },
      { entity: "I-PER", word: "Johnson" },
      { entity: "B-ORG", word: "Globe" },
      { entity: "I-ORG", word: "##x" },
      { entity: "I-ORG", word: "Corporation" },
    ]),
  );
  const out = await redactor(text);
  const counts = out.counts as Record<string, number>;
  assert.equal(out.text.includes("Katherine Johnson"), false, "person name must be redacted");
  assert.equal(out.text.includes("Globex Corporation"), false, "org (incl. ## subword) redacted");
  assert.equal(out.text, "Escalate the incident to [REDACTED:PER] at [REDACTED:ORG].");
  assert.equal(counts.pf_per, 1);
  assert.equal(counts.pf_org, 1);
});

test("offset-less: redacts every WORD-BOUNDARY occurrence of a detected surface", async () => {
  const redactor = await createPrivacyFilterRedactor(
    fakeTransformers([
      { entity: "B-PER", word: "Ada" },
      { entity: "I-PER", word: "Lovelace" },
    ]),
  );
  const out = await redactor("Ada Lovelace paired with Ada Lovelace again.");
  assert.equal(out.text, "[REDACTED:PER] paired with [REDACTED:PER] again.");
  assert.equal((out.counts as Record<string, number>).pf_per, 2);
});

test("offset-less: word boundary — a name must NOT corrupt a superstring", async () => {
  // Raw substring redaction would turn "Marketing" into "[REDACTED:PER]eting".
  // The standalone person "Mark" is redacted; "Marketing"/"Markdown" are not.
  const redactor = await createPrivacyFilterRedactor(
    fakeTransformers([{ entity: "B-PER", word: "Mark" }]),
  );
  const out = await redactor("Marketing lead Mark owns the Markdown docs.");
  assert.equal(out.text, "Marketing lead [REDACTED:PER] owns the Markdown docs.");
  assert.equal((out.counts as Record<string, number>).pf_per, 1);
});

test("offsets present: redacts ONLY the detected span, not other identical words", async () => {
  // The maintainer's precise flow. "Paris" appears twice; the model flags
  // only the person at 20..25, so the city earlier survives — something the
  // surface fallback (which redacts every occurrence) cannot do.
  const redactor = await createPrivacyFilterRedactor(
    fakeTransformers([{ entity: "B-PER", word: "Paris", start: 20, end: 25 }]),
  );
  const out = await redactor("We met in Paris and Paris signed off.");
  assert.equal(out.text, "We met in Paris and [REDACTED:PER] signed off.");
  assert.equal((out.counts as Record<string, number>).pf_per, 1);
});

test("offsets present: merges multi-token spans by min-start/max-end", async () => {
  const redactor = await createPrivacyFilterRedactor(
    fakeTransformers([
      { entity: "B-PER", word: "Bob", start: 3, end: 6 },
      { entity: "I-PER", word: "Smith", start: 7, end: 12 },
    ]),
  );
  const out = await redactor("Hi Bob Smith");
  assert.equal(out.text, "Hi [REDACTED:PER]");
  assert.equal((out.counts as Record<string, number>).pf_per, 1);
});

test("reconstructSurface rebuilds words and ## subwords", () => {
  assert.equal(reconstructSurface(["Katherine", "Johnson"]), "Katherine Johnson");
  assert.equal(reconstructSurface(["Globe", "##x", "Corporation"]), "Globex Corporation");
  assert.equal(reconstructSurface(["San", "Franc", "##isco"]), "San Francisco");
});

test("transient load failure: retries are bounded, then latched off", async () => {
  let importCalls = 0;
  const { restore } = captureWarns();
  try {
    const redactor = await createPrivacyFilterRedactor({
      importModule: async () => {
        importCalls += 1;
        throw new Error("ETIMEDOUT while fetching model weights");
      },
    });
    for (let i = 0; i < 200; i++) {
      await redactor(`segment abstract ${i}`);
    }
    assert.equal(importCalls, OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS);
  } finally {
    restore();
  }
});
