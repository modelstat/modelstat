/**
 * Pins the streaming contract behind the tray build's granular progress
 * (createLineSplitter in service.ts). SwiftPM output arrives in arbitrary
 * OS-sized chunks; the splitter must turn that back into whole lines so
 * the CLI prints clean "[2/3] Compiling…" progress instead of garbled
 * fragments. No swift toolchain involved — pure stream reassembly.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { createLineSplitter } from "./service.js";

test("createLineSplitter: emits one line per newline, trimming trailing CR/space", () => {
  const lines: string[] = [];
  const s = createLineSplitter((l) => lines.push(l));
  s.push("[1/3] Compiling ModelstatTray\n[2/3] Compiling main.swift\n");
  assert.deepEqual(lines, ["[1/3] Compiling ModelstatTray", "[2/3] Compiling main.swift"]);
});

test("createLineSplitter: reassembles a line split across chunk boundaries", () => {
  const lines: string[] = [];
  const s = createLineSplitter((l) => lines.push(l));
  // A single logical line delivered as three separate data chunks.
  s.push("[3/3] Lin");
  s.push("king modelstat-");
  s.push("tray\n");
  assert.deepEqual(lines, ["[3/3] Linking modelstat-tray"]);
});

test("createLineSplitter: drops blank lines but keeps content lines", () => {
  const lines: string[] = [];
  const s = createLineSplitter((l) => lines.push(l));
  s.push("\n\nBuild complete!\n\n");
  assert.deepEqual(lines, ["Build complete!"]);
});

test("createLineSplitter: flush emits a trailing newline-less line, once", () => {
  const lines: string[] = [];
  const s = createLineSplitter((l) => lines.push(l));
  s.push("▶ swift build -c release\n✓ Built build/ModelstatTray.app");
  assert.deepEqual(lines, ["▶ swift build -c release"]);
  s.flush();
  assert.deepEqual(lines, ["▶ swift build -c release", "✓ Built build/ModelstatTray.app"]);
  // Idempotent: a second flush (e.g. on a clean exit) emits nothing.
  s.flush();
  assert.equal(lines.length, 2);
});

test("createLineSplitter: flush on an empty buffer emits nothing", () => {
  const lines: string[] = [];
  const s = createLineSplitter((l) => lines.push(l));
  s.push("done\n");
  s.flush();
  assert.deepEqual(lines, ["done"]);
});
