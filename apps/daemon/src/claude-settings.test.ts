/**
 * Claude Code settings auto-config tests — installing the statusLine is
 * idempotent, composes with (and restores) a pre-existing foreign statusLine,
 * preserves unrelated settings, and writes valid JSON. Uses a throwaway
 * CLAUDE_CONFIG_DIR so the real ~/.claude is never touched.
 */
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { beforeEach, test } from "node:test";
import {
  claudeSettingsPath,
  installStatusline,
  isModelstatStatusLine,
  removeStatusline,
  STATUSLINE_COMMAND,
} from "./claude-settings.js";

function freshConfigDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "modelstat-claude-"));
  process.env.CLAUDE_CONFIG_DIR = dir;
  return dir;
}

beforeEach(() => {
  freshConfigDir();
});

function readJson(): Record<string, unknown> {
  return JSON.parse(readFileSync(claudeSettingsPath(), "utf8"));
}

test("install creates settings.json with our statusLine (no prior file)", () => {
  const r = installStatusline();
  assert.deepEqual(r, { kind: "installed", preserved: false });
  const s = readJson();
  assert.equal((s.statusLine as { command: string }).command, STATUSLINE_COMMAND);
  assert.equal((s.statusLine as { type: string }).type, "command");
});

test("install is idempotent — second call is a no-op", () => {
  installStatusline();
  const r2 = installStatusline();
  assert.deepEqual(r2, { kind: "already" });
});

test("install preserves unrelated settings", () => {
  writeFileSync(
    claudeSettingsPath(),
    JSON.stringify({ theme: "dark", permissions: { allow: ["Bash"] } }),
  );
  installStatusline();
  const s = readJson();
  assert.equal(s.theme, "dark");
  assert.deepEqual(s.permissions, { allow: ["Bash"] });
  assert.ok(isModelstatStatusLine(s.statusLine as { command: string }));
});

test("install stashes a foreign statusLine and uninstall restores it", () => {
  const foreign = { type: "command", command: "~/my/statusline.sh", padding: 4 };
  writeFileSync(claudeSettingsPath(), JSON.stringify({ statusLine: foreign }));

  const r = installStatusline();
  assert.deepEqual(r, { kind: "installed", preserved: true });
  let s = readJson();
  assert.ok(isModelstatStatusLine(s.statusLine as { command: string }));
  assert.deepEqual(s._modelstatPrevStatusLine, foreign);

  const rm = removeStatusline();
  assert.deepEqual(rm, { kind: "removed", restored: true });
  s = readJson();
  assert.deepEqual(s.statusLine, foreign);
  assert.equal(s._modelstatPrevStatusLine, undefined);
});

test("uninstall with no prior statusLine drops ours entirely", () => {
  installStatusline();
  const rm = removeStatusline();
  assert.deepEqual(rm, { kind: "removed", restored: false });
  const s = readJson();
  assert.equal(s.statusLine, undefined);
});

test("uninstall leaves a foreign statusLine we never installed over untouched", () => {
  const foreign = { type: "command", command: "~/other.sh" };
  writeFileSync(claudeSettingsPath(), JSON.stringify({ statusLine: foreign }));
  const rm = removeStatusline();
  assert.deepEqual(rm, { kind: "absent" });
  assert.deepEqual(readJson().statusLine, foreign);
});

test("a malformed settings.json surfaces an error rather than clobbering it", () => {
  writeFileSync(claudeSettingsPath(), "{ not valid json");
  const r = installStatusline();
  assert.equal(r.kind, "error");
  // The bad file is left intact (we never overwrote it).
  assert.equal(readFileSync(claudeSettingsPath(), "utf8"), "{ not valid json");
});
