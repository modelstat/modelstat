/**
 * Pins the install-time guarantee that the tray binary lands executable.
 *
 * The prebuilt vendor/ModelstatTray.app shipped in the npm tarball loses
 * its exec bit (`pnpm pack` normalises file modes and only keeps +x on
 * declared `bin` entries), and `cp -R` in installTrayApp faithfully
 * copies the non-executable file — so launchd fails to exec it and quits
 * with EX_CONFIG (78). installTrayApp must chmod the inner binary back to
 * executable. We redirect $HOME to a temp dir so the real ~/Applications
 * is never touched (os.homedir() honours $HOME on POSIX).
 */
import assert from "node:assert/strict";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { installTrayApp } from "./service.js";

test(
  "installTrayApp: makes the inner tray binary executable even when the source isn't",
  { skip: process.platform !== "darwin" },
  () => {
    const tmp = mkdtempSync(join(tmpdir(), "modelstat-install-"));
    const prevHome = process.env.HOME;
    process.env.HOME = tmp;
    try {
      // A source bundle whose inner binary is NOT executable — exactly the
      // -rw-r--r-- shape `pnpm pack` produces in the published tarball.
      const src = join(tmp, "src", "ModelstatTray.app");
      const innerSrc = join(src, "Contents", "MacOS", "modelstat-tray");
      mkdirSync(join(src, "Contents", "MacOS"), { recursive: true });
      writeFileSync(innerSrc, "#!/bin/sh\nexit 0\n");
      chmodSync(innerSrc, 0o644);
      assert.equal(statSync(innerSrc).mode & 0o111, 0, "precondition: source binary is not executable");

      const result = installTrayApp(src);
      assert.ok(result, "install should succeed on darwin");

      const installed = join(tmp, "Applications", "ModelstatTray.app", "Contents", "MacOS", "modelstat-tray");
      assert.notEqual(statSync(installed).mode & 0o111, 0, "installed binary must be executable");
    } finally {
      if (prevHome === undefined) delete process.env.HOME;
      else process.env.HOME = prevHome;
      rmSync(tmp, { recursive: true, force: true });
    }
  },
);

