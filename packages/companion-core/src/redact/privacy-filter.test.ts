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
import { createPrivacyFilterRedactor } from "./privacy-filter.js";

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
