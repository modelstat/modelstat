/**
 * Pins the daemon's friendly-name launcher contract.
 *
 * macOS shows a process's name from the FILENAME of the running binary, not
 * from Node's process.title — so the daemon launches through a link named
 * "modelstat agent" to make Activity Monitor read that instead of "node".
 * What MUST stay correct: the name (≤16 chars so p_comm doesn't truncate),
 * that the launchd plist execs the launcher rather than bare node, and that
 * the link is a zero-cost hardlink to the same inode. $HOME is redirected so
 * no real file is touched.
 */
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { platform, tmpdir } from "node:os";
import { basename, join } from "node:path";
import { test } from "node:test";
import {
  DAEMON_LAUNCHER_NAME,
  daemonPlistContents,
  ensureDaemonLauncher,
  parseRpathDylibs,
} from "./service.js";

// The link trick is macOS-specific; elsewhere process.title already renames the
// process, so ensureDaemonLauncher is a no-op that returns node unchanged.
const macOnly = { skip: platform() !== "darwin" };

test("DAEMON_LAUNCHER_NAME is the agreed name and fits p_comm's 16-char cap", () => {
  // A longer name would be truncated by the kernel (e.g. "modelstat watcher"
  // → "modelstat watche"); "modelstat agent" is 15 chars and shows in full.
  assert.equal(DAEMON_LAUNCHER_NAME, "modelstat agent");
  assert.ok(DAEMON_LAUNCHER_NAME.length <= 16, "must fit the 16-char p_comm cap");
});

test("daemonPlistContents: execs the launcher directly, then the CLI's `start`", () => {
  const launcher = "/Users/somebody/.modelstat/bin/modelstat agent";
  const cli = "/Users/somebody/.modelstat/bin/modelstat.mjs";
  const plist = daemonPlistContents(launcher, cli);
  // The launcher (not bare node) must be argv[0] — that's what renames the
  // process in Activity Monitor.
  assert.ok(plist.includes(`<string>${launcher}</string>`), "launcher must be argv[0]");
  assert.ok(plist.includes(`<string>${cli}</string>`), "CLI bundle must be argv[1]");
  assert.match(plist, /<string>start<\/string>/);
  assert.match(plist, /<key>Label<\/key><string>ai\.modelstat\.daemon<\/string>/);
});

test(
  "ensureDaemonLauncher: hardlinks a 'modelstat agent' launcher at zero disk cost",
  macOnly,
  () => {
    const prevHome = process.env.HOME;
    const home = mkdtempSync(join(tmpdir(), "ms-launcher-"));
    process.env.HOME = home;
    try {
      // A stand-in "node" on the same volume as the fake HOME, so the hardlink
      // can't hit EXDEV and fall back to a copy.
      const fakeNode = join(home, "node");
      writeFileSync(fakeNode, "#!/bin/sh\ntrue\n", { mode: 0o755 });

      const launcher = ensureDaemonLauncher(fakeNode);

      assert.equal(basename(launcher), "modelstat agent");
      assert.ok(existsSync(launcher), "launcher must exist");
      // Hardlink → same inode as the source → zero extra bytes on disk.
      assert.equal(
        statSync(launcher).ino,
        statSync(fakeNode).ino,
        "launcher must be a hardlink (same inode), not a copy",
      );
    } finally {
      rmSync(home, { recursive: true, force: true });
      if (prevHome === undefined) delete process.env.HOME;
      else process.env.HOME = prevHome;
    }
  },
);

test("ensureDaemonLauncher: re-points an existing launcher to the current node", macOnly, () => {
  const prevHome = process.env.HOME;
  const home = mkdtempSync(join(tmpdir(), "ms-launcher-"));
  process.env.HOME = home;
  try {
    // First install links to the old node... Runnable stand-in nodes (exit 0)
    // so the new runnability preflight passes and we exercise the re-point path,
    // not the bare-node fallback.
    const oldNode = join(home, "node-old");
    writeFileSync(oldNode, "#!/bin/sh\ntrue\n", { mode: 0o755 });
    const first = ensureDaemonLauncher(oldNode);
    assert.equal(statSync(first).ino, statSync(oldNode).ino);

    // ...a node upgrade gives node a NEW inode; the launcher must follow it,
    // not keep executing the old one forever.
    const newNode = join(home, "node-new");
    writeFileSync(newNode, "#!/bin/sh\ntrue\n", { mode: 0o755 });
    const second = ensureDaemonLauncher(newNode);
    assert.equal(
      statSync(second).ino,
      statSync(newNode).ino,
      "launcher must re-point to the upgraded node",
    );
  } finally {
    rmSync(home, { recursive: true, force: true });
    if (prevHome === undefined) delete process.env.HOME;
    else process.env.HOME = prevHome;
  }
});

test(
  "ensureDaemonLauncher: falls back to bare node when the launcher can't run",
  macOnly,
  () => {
    const prevHome = process.env.HOME;
    const home = mkdtempSync(join(tmpdir(), "ms-launcher-"));
    process.env.HOME = home;
    try {
      // Stand-in for a dyld-broken launcher: a "node" that exits non-zero, so
      // the runnability preflight fails. We must NOT hand launchd a binary it
      // would abort-loop on — fall back to the real node path instead.
      const brokenNode = join(home, "node-broken");
      writeFileSync(brokenNode, "#!/bin/sh\nexit 1\n", { mode: 0o755 });

      const result = ensureDaemonLauncher(brokenNode);

      assert.equal(result, brokenNode, "must fall back to bare node when the launcher can't run");
      const launcher = join(home, ".modelstat", "bin", DAEMON_LAUNCHER_NAME);
      assert.ok(
        !existsSync(launcher),
        "a non-runnable launcher must be removed, not left for launchd to abort-loop on",
      );
    } finally {
      rmSync(home, { recursive: true, force: true });
      if (prevHome === undefined) delete process.env.HOME;
      else process.env.HOME = prevHome;
    }
  },
);

test("parseRpathDylibs: keeps @rpath dylibs, drops absolute kegs, dedupes", () => {
  const otool = [
    "/Users/x/.modelstat/bin/modelstat agent:",
    "\t@rpath/libnode.127.dylib (compatibility version 0.0.0, current version 0.0.0)",
    "\t/usr/lib/libz.1.dylib (compatibility version 1.0.0, current version 1.2.12)",
    "\t/opt/homebrew/opt/libuv/lib/libuv.1.dylib (compatibility version 1.0.0, current version 1.0.0)",
    "\t@rpath/libnode.127.dylib (compatibility version 0.0.0, current version 0.0.0)",
  ].join("\n");
  // Only the relative @rpath dep needs to travel with a relocated launcher; the
  // absolute /usr/lib + /opt/homebrew kegs resolve wherever the launcher lives.
  assert.deepEqual(parseRpathDylibs(otool), ["libnode.127.dylib"]);
});

test("parseRpathDylibs: empty for a node with no @rpath deps (statically linked)", () => {
  assert.deepEqual(parseRpathDylibs("some-node:\n\t/usr/lib/libSystem.B.dylib (…)\n"), []);
});
