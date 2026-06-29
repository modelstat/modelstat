/**
 * Pins the tray auto-start contract — the launchd agent that replaces the
 * old self-registering Login Item (the chicken-and-egg trap that left the
 * tray dead after any reboot/crash).
 *
 * We test the PURE plist rendering + paths, not the launchctl orchestration:
 * actually running installTrayAutostart() would load a bogus agent into the
 * real user session and `pkill` the developer's own running tray. The
 * launchctl wiring mirrors the daemon's macInstall() (also not unit-tested)
 * and is covered by the end-to-end check on macOS. What MUST stay correct is
 * the plist itself — that's what encodes "start at login, restart on crash,
 * stay dead on user-quit". $HOME is redirected so no real file is touched.
 */
import assert from "node:assert/strict";
import { join } from "node:path";
import { test } from "node:test";
import {
  SERVICE_LABEL,
  TRAY_SERVICE_LABEL,
  trayPlistContents,
  trayPlistPath,
} from "./service.js";

const BIN = "/Users/somebody/Applications/ModelstatTray.app/Contents/MacOS/modelstat-tray";

test("tray agent has its OWN label, distinct from the daemon's", () => {
  // A shared label would make installing one boot the other out.
  assert.equal(TRAY_SERVICE_LABEL, "ai.modelstat.tray");
  assert.notEqual(TRAY_SERVICE_LABEL, SERVICE_LABEL);
});

test("trayPlistContents: execs the given tray binary directly", () => {
  const plist = trayPlistContents(BIN);
  assert.match(plist, /<key>Label<\/key><string>ai\.modelstat\.tray<\/string>/);
  // ProgramArguments must be the GUI binary itself (the whole point of the
  // finding: launchd can exec it directly — no `open`, no node wrapper).
  assert.ok(plist.includes(`<string>${BIN}</string>`), "binary must appear in ProgramArguments");
});

test("trayPlistContents: RunAtLoad brings the icon back at every login", () => {
  assert.match(trayPlistContents(BIN), /<key>RunAtLoad<\/key><true\/>/);
});

test("trayPlistContents: KeepAlive restarts a crash but NOT a clean user-quit", () => {
  // This exact shape is load-bearing. KeepAlive={SuccessfulExit:false} means
  // launchd relaunches only on a non-zero/abnormal exit. The tray's Quit menu
  // exits 0, so it stays dead for the session (no instant-relaunch fight);
  // a crash exits non-zero, so it comes back. A plain <true/> here would
  // re-fight the user every time they quit — guard against that regression.
  const plist = trayPlistContents(BIN);
  assert.match(plist, /<key>KeepAlive<\/key>\s*<dict><key>SuccessfulExit<\/key><false\/><\/dict>/);
  assert.ok(!/<key>KeepAlive<\/key><true\/>/.test(plist), "KeepAlive must not be unconditional");
});

test("trayPlistContents: logs go to ~/.modelstat/logs/tray-*.log", () => {
  const prevHome = process.env.HOME;
  process.env.HOME = "/tmp/ms-home-xyz";
  try {
    const plist = trayPlistContents(BIN);
    assert.ok(plist.includes("/tmp/ms-home-xyz/.modelstat/logs/tray-out.log"));
    assert.ok(plist.includes("/tmp/ms-home-xyz/.modelstat/logs/tray-err.log"));
  } finally {
    if (prevHome === undefined) delete process.env.HOME;
    else process.env.HOME = prevHome;
  }
});

test("trayPlistPath: ~/Library/LaunchAgents/ai.modelstat.tray.plist", () => {
  const prevHome = process.env.HOME;
  process.env.HOME = "/tmp/ms-home-xyz";
  try {
    assert.equal(
      trayPlistPath(),
      join("/tmp/ms-home-xyz", "Library", "LaunchAgents", "ai.modelstat.tray.plist"),
    );
  } finally {
    if (prevHome === undefined) delete process.env.HOME;
    else process.env.HOME = prevHome;
  }
});
