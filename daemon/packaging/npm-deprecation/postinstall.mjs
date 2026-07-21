#!/usr/bin/env node
// The bridge for the FINAL npm `modelstat` version (feature §22, executed at M9).
// The daemon moved off npm to native binaries; old Node daemons self-update via
// `npm i -g modelstat@latest`, which lands THIS version — whose postinstall
// migrates them onto the native installer. Prints prominently, then best-effort
// runs the platform installer. Never throws (a failed migration must not wedge
// the npm install; the printed command is the manual fallback).
import { spawnSync } from "node:child_process";
import { platform } from "node:os";

const CURL = "curl -fsSL https://modelstat.ai/install.sh | sh";
const PS = "irm https://modelstat.ai/install.ps1 | iex";
const isWin = platform() === "win32";

console.log("\n  modelstat has moved off npm to a small native binary (no Node needed).");
console.log("  Migrating this install to the native daemon…\n");
console.log(`  If anything goes wrong, run it yourself:\n    ${isWin ? PS : CURL}\n`);

try {
  if (isWin) {
    spawnSync("powershell", ["-NoProfile", "-Command", PS], { stdio: "inherit" });
  } else {
    spawnSync("sh", ["-c", CURL], { stdio: "inherit" });
  }
} catch {
  // Best-effort — the printed command above is the manual path.
}
