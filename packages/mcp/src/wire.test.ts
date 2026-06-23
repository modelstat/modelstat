/**
 * Wire tests — deterministic merge behaviour against a temp HOME. We never
 * shell out to `claude` here (includeClaudeCode: false); the JSON/TOML merges
 * and per-OS path resolution are what matter.
 */
import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { jsonTargets, runWire, wireCodex, wireJsonTarget } from "./wire.js";

function tmpHome(): string {
  return mkdtempSync(join(tmpdir(), "ms-wire-"));
}
function target(home: string, plat: NodeJS.Platform, name: string) {
  const t = jsonTargets(home, plat).find((x) => x.name === name);
  assert.ok(t, `target ${name} exists`);
  return t;
}

test("VS Code uses the `servers` key with a stdio command entry", () => {
  const home = tmpHome();
  const t = target(home, "darwin", "VS Code");
  mkdirSync(t.detect, { recursive: true });
  assert.equal(wireJsonTarget(t), "configured");
  const cfg = JSON.parse(readFileSync(t.file, "utf8"));
  assert.deepEqual(cfg.servers.modelstat, { command: "npx", args: ["-y", "@modelstat/mcp"] });
  assert.match(t.file, /User\/mcp\.json$/);
  // idempotent
  assert.equal(wireJsonTarget(t), "already");
});

test("an undetected tool is skipped and no file is created", () => {
  const home = tmpHome();
  const t = target(home, "darwin", "Cursor");
  assert.equal(wireJsonTarget(t), "absent");
  assert.equal(existsSync(t.file), false);
});

test("merge is non-destructive — sibling servers and other keys survive", () => {
  const home = tmpHome();
  const t = target(home, "darwin", "Claude Desktop");
  mkdirSync(t.detect, { recursive: true });
  writeFileSync(t.file, JSON.stringify({ mcpServers: { other: { command: "x" } }, theme: "dark" }));
  assert.equal(wireJsonTarget(t), "configured");
  const cfg = JSON.parse(readFileSync(t.file, "utf8"));
  assert.deepEqual(cfg.mcpServers.other, { command: "x" });
  assert.equal(cfg.theme, "dark");
  assert.deepEqual(cfg.mcpServers.modelstat, { command: "npx", args: ["-y", "@modelstat/mcp"] });
});

test("Zed uses context_servers with the custom-source shape", () => {
  const home = tmpHome();
  const t = target(home, "darwin", "Zed");
  mkdirSync(t.detect, { recursive: true });
  assert.equal(wireJsonTarget(t), "configured");
  const cfg = JSON.parse(readFileSync(t.file, "utf8"));
  assert.deepEqual(cfg.context_servers.modelstat, {
    source: "custom",
    command: "npx",
    args: ["-y", "@modelstat/mcp"],
    env: {},
  });
});

test("Codex appends a TOML table, preserving existing content + idempotent", () => {
  const home = tmpHome();
  mkdirSync(join(home, ".codex"), { recursive: true });
  writeFileSync(join(home, ".codex", "config.toml"), 'model = "o3"\n');
  assert.equal(wireCodex(home), "configured");
  const toml = readFileSync(join(home, ".codex", "config.toml"), "utf8");
  assert.match(toml, /model = "o3"/);
  assert.match(toml, /\[mcp_servers\.modelstat\]/);
  assert.equal(wireCodex(home), "already");
});

test("Windows resolves app-support configs under APPDATA", () => {
  const saved = process.env.APPDATA;
  process.env.APPDATA = "C:\\Users\\x\\AppData\\Roaming";
  try {
    const t = target("C:\\Users\\x", "win32", "VS Code");
    assert.match(t.file.replace(/\\/g, "/"), /AppData\/Roaming\/Code\/User\/mcp\.json$/);
    const cd = target("C:\\Users\\x", "win32", "Claude Desktop");
    assert.match(cd.file.replace(/\\/g, "/"), /AppData\/Roaming\/Claude\/claude_desktop_config\.json$/);
  } finally {
    if (saved === undefined) delete process.env.APPDATA;
    else process.env.APPDATA = saved;
  }
});

test("runWire returns 0 and reports per-tool status without throwing", async () => {
  const home = tmpHome();
  // VS Code present, nothing else
  const vscode = target(home, "darwin", "VS Code");
  mkdirSync(vscode.detect, { recursive: true });
  const lines: string[] = [];
  const code = await runWire({ home, platform: "darwin", includeClaudeCode: false, log: (l) => lines.push(l) });
  assert.equal(code, 0);
  assert.ok(lines.some((l) => /VS Code — configured/.test(l)));
  assert.ok(lines.some((l) => /Cursor — not detected/.test(l)));
});
