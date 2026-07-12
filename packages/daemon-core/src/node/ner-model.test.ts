/**
 * ensureNerModel reports LIVE only when the NER model actually scrubs a sentinel
 * PERSON — a pass-through (no detections) or a missing package reports false, and
 * it never throws, so the connect-time warm can be best-effort.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { ensureNerModel } from "./ner-model.js";

/** A fake @huggingface/transformers whose classifier returns a fixed token list. */
function fakeTransformers(tokens: { entity: string; word: string }[]) {
  return async () => ({ pipeline: async () => async (_t: string) => tokens });
}

function silenceWarns(): () => void {
  const orig = console.warn;
  console.warn = () => {};
  return () => {
    console.warn = orig;
  };
}

test("ensureNerModel: LIVE when the model scrubs the sentinel PERSON", async () => {
  const live = await ensureNerModel({
    importModule: fakeTransformers([
      { entity: "B-PER", word: "Katherine" },
      { entity: "I-PER", word: "Johnson" },
    ]),
  });
  assert.equal(live, true);
});

test("ensureNerModel: NOT live when the model detects nothing (silent pass-through)", async () => {
  const live = await ensureNerModel({ importModule: fakeTransformers([]) });
  assert.equal(live, false);
});

test("ensureNerModel: NOT live and never throws when the package is missing", async () => {
  const restore = silenceWarns();
  try {
    const live = await ensureNerModel({
      importModule: async () => {
        const err = new Error("Cannot find package '@huggingface/transformers'") as Error & {
          code?: string;
        };
        err.code = "ERR_MODULE_NOT_FOUND";
        throw err;
      },
    });
    assert.equal(live, false);
  } finally {
    restore();
  }
});
