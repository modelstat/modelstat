/**
 * Regression tests for the embedder's optional-dep loading.
 *
 * The 2026-06-11 daemon OOM: with @huggingface/transformers missing
 * from the staged runtime, every embed() call re-ran a FAILING dynamic
 * import() and logged an identical warn line — ~5M times across a
 * full-corpus reprocess, growing err.log to ~1 GB and the V8 heap to
 * the 8 GB ceiling. These tests pin the contract that a missing
 * package latches the loader off after ONE import attempt and ONE
 * warning, no matter how many times embed() runs.
 */

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  isMissingOptionalModuleError,
  OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS,
} from "../optional-module.js";
import { __setImporterForTests, createTransformersJsEmbedder } from "./transformersjs-embed.js";

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

test("missing package: one import attempt + one warn across thousands of embed calls", async () => {
  let importCalls = 0;
  __setImporterForTests(async () => {
    importCalls += 1;
    throw missingPackageError();
  });
  const { warns, restore } = captureWarns();
  try {
    const embed = createTransformersJsEmbedder();
    for (let i = 0; i < 2000; i++) {
      assert.deepEqual(await embed(`turn surface ${i}`), []);
    }
    assert.equal(importCalls, 1, "failing import must not sit on the per-turn hot path");
    assert.equal(warns.length, 1, "unavailability must be warned exactly once per process");
  } finally {
    restore();
    __setImporterForTests(null);
  }
});

test("transient load failure: retries are bounded, then latched off", async () => {
  let importCalls = 0;
  __setImporterForTests(async () => {
    importCalls += 1;
    throw new Error("ECONNRESET while fetching model weights");
  });
  const { restore } = captureWarns();
  try {
    const embed = createTransformersJsEmbedder();
    for (let i = 0; i < 500; i++) {
      assert.deepEqual(await embed(`turn surface ${i}`), []);
    }
    assert.equal(importCalls, OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS);
  } finally {
    restore();
    __setImporterForTests(null);
  }
});

test("transient failure then success: loader recovers and embeds", async () => {
  let importCalls = 0;
  __setImporterForTests(async () => {
    importCalls += 1;
    if (importCalls === 1) throw new Error("socket hang up");
    return {
      pipeline: async () => async () => ({
        data: Float32Array.from([0.1, 0.2, 0.3]),
        dims: [1, 3],
      }),
    };
  });
  const { restore } = captureWarns();
  try {
    const embed = createTransformersJsEmbedder();
    assert.deepEqual(await embed("first"), []);
    const v = await embed("second");
    assert.equal(v.length, 3);
    assert.equal(importCalls, 2);
  } finally {
    restore();
    __setImporterForTests(null);
  }
});

test("isMissingOptionalModuleError classifies resolver failures", () => {
  assert.equal(isMissingOptionalModuleError(missingPackageError()), true);
  const byMessage = new Error("Cannot find module 'foo'");
  assert.equal(isMissingOptionalModuleError(byMessage), true);
  assert.equal(isMissingOptionalModuleError(new Error("ECONNRESET")), false);
});
