import { strict as assert } from "node:assert";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  DEFAULT_SUMMARIZER_MODE,
  parseSummarizerMode,
  readModeSettings,
  writeMode,
  writeSelfHosted,
} from "./mode-settings.js";

/** Run `fn` with MODELSTAT_HOME pointed at a throwaway dir. mode-settings reads
 * fresh from disk each call, so there's no cache to reset. */
function withTempHome(fn: (dir: string) => void): void {
  const saved = process.env.MODELSTAT_HOME;
  const dir = mkdtempSync(join(tmpdir(), "modelstat-mode-"));
  process.env.MODELSTAT_HOME = dir;
  try {
    fn(dir);
  } finally {
    if (saved === undefined) delete process.env.MODELSTAT_HOME;
    else process.env.MODELSTAT_HOME = saved;
    rmSync(dir, { recursive: true, force: true });
  }
}

test("defaults to cloud on a fresh install (no mode.json)", () => {
  withTempHome(() => {
    assert.equal(DEFAULT_SUMMARIZER_MODE, "cloud");
    assert.deepEqual(readModeSettings(), { mode: "cloud", selfHostedUrl: "", selfHostedModel: "" });
  });
});

test("mode + self-hosted endpoint round-trip; each write preserves the other field", () => {
  withTempHome(() => {
    writeMode("self-hosted");
    writeSelfHosted("https://llm.acme.internal/v1", "qwen2.5-7b-instruct");
    const s = readModeSettings();
    assert.equal(s.mode, "self-hosted");
    assert.equal(s.selfHostedUrl, "https://llm.acme.internal/v1");
    assert.equal(s.selfHostedModel, "qwen2.5-7b-instruct");
    // writeMode must preserve the stored endpoint (used when flipping back).
    writeMode("local");
    const s2 = readModeSettings();
    assert.equal(s2.mode, "local");
    assert.equal(s2.selfHostedUrl, "https://llm.acme.internal/v1");
  });
});

test("a garbage persisted mode falls back to the cloud default", () => {
  withTempHome((dir) => {
    writeFileSync(join(dir, "mode.json"), JSON.stringify({ mode: "banana" }));
    assert.equal(readModeSettings().mode, "cloud");
  });
});

test("parseSummarizerMode normalises + rejects", () => {
  assert.equal(parseSummarizerMode("cloud"), "cloud");
  assert.equal(parseSummarizerMode("  LOCAL "), "local");
  assert.equal(parseSummarizerMode("Self-Hosted"), "self-hosted");
  assert.equal(parseSummarizerMode("banana"), null);
  assert.equal(parseSummarizerMode(""), null);
  assert.equal(parseSummarizerMode(undefined), null);
});
