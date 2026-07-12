/**
 * The shared Transformers.js cache dir must be computed identically by the
 * connect CLI and the daemon (so a warm from one is a cache hit for the other),
 * and it mirrors the llama model dir — including the MODELSTAT_MODELS_DIR
 * override — so both models live under one root.
 */
import assert from "node:assert/strict";
import { homedir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { transformersCacheDir } from "./transformers-cache.js";

test("defaults to ~/.modelstat/models/hf", () => {
  const prev = process.env.MODELSTAT_MODELS_DIR;
  delete process.env.MODELSTAT_MODELS_DIR;
  try {
    assert.equal(transformersCacheDir(), join(homedir(), ".modelstat", "models", "hf"));
  } finally {
    if (prev !== undefined) process.env.MODELSTAT_MODELS_DIR = prev;
  }
});

test("honours the MODELSTAT_MODELS_DIR override (same knob as the llama model dir)", () => {
  const prev = process.env.MODELSTAT_MODELS_DIR;
  process.env.MODELSTAT_MODELS_DIR = "/tmp/models-override";
  try {
    assert.equal(transformersCacheDir(), join("/tmp/models-override", "hf"));
  } finally {
    if (prev === undefined) delete process.env.MODELSTAT_MODELS_DIR;
    else process.env.MODELSTAT_MODELS_DIR = prev;
  }
});
