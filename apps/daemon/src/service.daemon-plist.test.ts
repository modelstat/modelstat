/**
 * Pins the daemon's launch + install contract:
 *   · the launchd plist execs node IN PLACE (process.execPath), then the
 *     bundled CLI's `start`; and
 *   · install clears the retired "modelstat agent" launcher (+ orphaned
 *     libnode) from older versions.
 *
 * We deliberately do NOT rename/relocate node for a prettier Activity Monitor
 * name — relocating Homebrew's node orphaned its separate libnode and bricked
 * self-updates (dyld: Library not loaded: @rpath/libnode.<v>.dylib). Running
 * node where it lives can't break that way; off macOS, process.title (set in
 * cli.ts cmdStart) shows "modelstat agent" in ps/top — on macOS it stays "node".
 */
import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { platform, tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { cleanupStaleLauncher, daemonPlistContents } from "./service.js";

// cleanupStaleLauncher only touches macOS artifacts; elsewhere it's a no-op.
const macOnly = { skip: platform() !== "darwin" };

test("daemonPlistContents: execs node directly, then the CLI's `start`", () => {
  const node = "/opt/homebrew/opt/node@22/bin/node";
  const cli = "/Users/somebody/.modelstat/bin/modelstat.mjs";
  const plist = daemonPlistContents(node, cli);
  // node (in place, not a renamed copy) is argv[0]; the bundle is argv[1].
  assert.ok(plist.includes(`<string>${node}</string>`), "node must be argv[0]");
  assert.ok(plist.includes(`<string>${cli}</string>`), "CLI bundle must be argv[1]");
  assert.match(plist, /<string>start<\/string>/);
  assert.match(plist, /<key>Label<\/key><string>ai\.modelstat\.daemon<\/string>/);
  // Regression guard: never exec a renamed "modelstat agent" launcher again.
  assert.ok(!plist.includes("modelstat agent"), "must not exec a renamed launcher");
});

test(
  "cleanupStaleLauncher: removes the retired launcher + orphaned libnode, keeps the bundle",
  macOnly,
  () => {
    const prevHome = process.env.HOME;
    const home = mkdtempSync(join(tmpdir(), "ms-cleanup-"));
    process.env.HOME = home;
    try {
      const bin = join(home, ".modelstat", "bin");
      mkdirSync(bin, { recursive: true });
      writeFileSync(join(bin, "modelstat agent"), "retired launcher");
      writeFileSync(join(bin, "libnode.127.dylib"), "orphaned dylib");
      writeFileSync(join(bin, "modelstat.mjs"), "the real bundle"); // must survive

      cleanupStaleLauncher();

      assert.ok(!existsSync(join(bin, "modelstat agent")), "retired launcher must be removed");
      assert.ok(!existsSync(join(bin, "libnode.127.dylib")), "orphaned libnode must be removed");
      assert.ok(existsSync(join(bin, "modelstat.mjs")), "the CLI bundle must be left intact");
    } finally {
      rmSync(home, { recursive: true, force: true });
      if (prevHome === undefined) delete process.env.HOME;
      else process.env.HOME = prevHome;
    }
  },
);
