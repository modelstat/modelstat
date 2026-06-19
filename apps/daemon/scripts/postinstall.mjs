#!/usr/bin/env node
/**
 * Post-install hook for the @modelstat/daemon CLI.
 *
 * Runs after `npm install -g modelstat` (or `npx modelstat` cache
 * population) and downloads the bundled summariser GGUF (~2.7 GB)
 * with visible progress, so the user sees the heavy download
 * attached to the install they just kicked off — not as a surprise
 * 7-minute hang the first time they run `modelstat scan`.
 *
 * Skipped when:
 *   - Running inside this repo's pnpm workspace (we don't want every
 *     dev `pnpm install` to pull 2.7 GB; the developer can opt in by
 *     running `modelstat connect` once or pre-pulling the model).
 *   - `MODELSTAT_SKIP_POSTINSTALL=1` is set (CI escape hatch, or for
 *     packagers who handle the model out-of-band).
 *   - `CI=true` is set (most CI providers; avoids blocking automated
 *     installs on a multi-GB pull).
 *   - stdout isn't a TTY AND CI isn't set (likely a scripted
 *     install that doesn't want long-running side-effects).
 *
 * In all skip cases the model still downloads lazily on first
 * `modelstat scan` — the daemon always preflights the summariser
 * before producing any segments (see src/pipeline.ts), so users in
 * skip-mode just see the download then, instead of now.
 */

import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

function inThisMonorepo() {
  // Walk up from this script looking for the repo's pnpm-workspace.yaml.
  // When npm installs us into a global node_modules, this file isn't an
  // ancestor — so we know we're in a real install and should download.
  let dir = dirname(fileURLToPath(import.meta.url));
  for (let i = 0; i < 10; i++) {
    if (existsSync(join(dir, "pnpm-workspace.yaml"))) return true;
    const up = resolve(dir, "..");
    if (up === dir) return false;
    dir = up;
  }
  return false;
}

async function main() {
  if (process.env.MODELSTAT_SKIP_POSTINSTALL === "1") {
    console.log(
      "[modelstat] postinstall skipped (MODELSTAT_SKIP_POSTINSTALL=1)",
    );
    return;
  }
  if (inThisMonorepo()) {
    // Dev environment — don't pull 2.7 GB on every workspace install,
    // and don't muck with the developer's running daemon.
    return;
  }

  // ── Always-run path: refresh the existing service if there is one ─
  // The bundle at ~/.modelstat/bin/modelstat.mjs and its sibling
  // node_modules need to track every upgrade, otherwise the daemon
  // launches against stale code (or, worse, code that imports a
  // native module the local install doesn't have anymore). This MUST
  // run regardless of TTY/CI — the previous behaviour gated it on
  // an interactive shell, which left every npm-cache / npx-prewarm
  // install with a half-installed bundle the daemon couldn't load.
  await rebootServiceIfInstalled();

  // ── TTY-only path: pre-download the heavy summariser model ───────
  // The 2.7 GB GGUF is the only thing we want to skip in non-TTY
  // installs (CI, npm cache, packagers). The daemon always preflights
  // the summariser before producing segments (see src/pipeline.ts),
  // so users in skip-mode just see the download then, instead of now.
  const skipModelDownload =
    process.env.CI === "true" ||
    process.env.CI === "1" ||
    !process.stdout.isTTY;
  if (skipModelDownload) {
    console.log(
      "[modelstat] non-interactive install — summariser model will download lazily on first scan",
    );
    return;
  }

  // Resolve the helper from the workspace package the bundle uses.
  // daemon-core is bundled into dist/cli.mjs by tsup, but we need
  // the helper as a standalone import here (postinstall runs against
  // the unbundled source layout).
  let ensureLlamaModel;
  let defaultLlamaConfig;
  try {
    ({ ensureLlamaModel, defaultLlamaConfig } = await import(
      "@modelstat/daemon-core/node"
    ));
  } catch (err) {
    console.warn(
      `[modelstat] postinstall: couldn't import summariser helper (${err && err.message ? err.message : err}); model will download lazily on first scan`,
    );
    return;
  }

  console.log("");
  console.log("━".repeat(60));
  console.log("  Pre-downloading the local summariser model");
  console.log("");
  console.log("  modelstat ships with a small LLM that runs ON YOUR MACHINE");
  console.log("  to summarise every coding session. The model is ~2.7 GB —");
  console.log("  pulling it now so the first `modelstat scan` is instant.");
  console.log("━".repeat(60));
  console.log("");

  try {
    await ensureLlamaModel(defaultLlamaConfig());
    console.log("");
    console.log("[modelstat] ✓ summariser ready");
  } catch (err) {
    // Don't fail the npm install. The model will retry-download on
    // first scan; better to leave the user with a working binary
    // than to abort because their network blipped.
    console.warn(
      `[modelstat] ⚠ couldn't pre-download summariser (${err && err.message ? err.message : err})`,
    );
    console.warn(
      "[modelstat]   the model will download lazily on first `modelstat scan` instead",
    );
  }
  console.log(
    "[modelstat] all set — your dashboard already has the new daemon running",
  );
}

async function rebootServiceIfInstalled() {
  const stateDir = join(homedir(), ".modelstat");
  const launchdPlist = join(
    homedir(),
    "Library",
    "LaunchAgents",
    "ai.modelstat.daemon.plist",
  );
  const systemdUnit = join(
    process.env.XDG_CONFIG_HOME ?? join(homedir(), ".config"),
    "systemd",
    "user",
    "modelstat.service",
  );
  const hasService = existsSync(launchdPlist) || existsSync(systemdUnit);
  if (!hasService) {
    console.log(
      "[modelstat] no background service installed — run `modelstat connect` to set one up",
    );
    return;
  }

  // Locate the freshly-installed bundle in this package. From
  // scripts/postinstall.mjs → ../dist/cli.mjs.
  const here = dirname(fileURLToPath(import.meta.url));
  const freshBundle = join(here, "..", "dist", "cli.mjs");
  if (!existsSync(freshBundle)) {
    console.warn(
      "[modelstat] couldn't find new bundle — service refresh skipped",
    );
    return;
  }

  // Spawn the FRESH bundle to do the actual restart through its own
  // service.ts — that's the same code that knows how to talk to
  // launchd / systemd on each platform. We can't import service.ts
  // directly here (postinstall is a plain .mjs without TS resolution),
  // but spawning the new bundle gives us the new behaviour.
  const { spawnSync } = await import("node:child_process");
  console.log("[modelstat] refreshing background service…");

  // Stop the SERVICE (launchctl bootout / systemctl --user disable)
  // via the fresh bundle's own knowledge of each platform.
  spawnSync(process.execPath, [freshBundle, "stop"], {
    stdio: "ignore",
    timeout: 30_000,
  });

  // Belt-and-braces: kill any stray daemon process that the service
  // supervisor didn't reap (stale lock, killed parent, KeepAlive
  // race during the bundle swap below). Lock file at
  // ~/.modelstat/daemon.lock has the live PID; SIGTERM it gently
  // first, escalate to SIGKILL if it's still around 2 s later.
  await killStaleDaemon(stateDir);

  // Re-stage the installed bundle AND its native summariser runtime in
  // ONE canonical step by spawning the freshly-unpacked bundle's own
  // `_setup-runtime` command. That runs the SAME service.ts staging the
  // `modelstat connect` path uses: it copies dist/cli.mjs over
  // ~/.modelstat/bin/modelstat.mjs and `npm install`s the complete
  // node-llama-cpp closure (incl. this platform's prebuilt binary) into
  // ~/.modelstat/bin/node_modules. freshBundle runs from the just-
  // unpacked npm tree, so its installNativeRuntime() pins the version.
  //
  // This replaces the old hand-copy + symlink-the-npm-cache approach,
  // whose symlinks dangled the moment the cache/global was pruned — the
  // opposite of self-contained. One mechanism, every platform.
  console.log("[modelstat] staging new bundle + native runtime…");
  const setup = spawnSync(process.execPath, [freshBundle, "_setup-runtime"], {
    stdio: "inherit",
    timeout: 300_000,
  });
  if (setup.status !== 0) {
    console.warn(
      `[modelstat] couldn't stage the new bundle/runtime (exit ${setup.status ?? "?"}); the service may still be on the previous build`,
    );
  }

  // Start back up. `modelstat start` re-runs preflight (incl. the
  // processing-version reconcile that wipes cursors when we ship a
  // new pipeline), so the upgrade picks up the new behaviour
  // immediately. Detached so the daemon survives this script.
  const { spawn } = await import("node:child_process");
  const child = spawn(process.execPath, [freshBundle, "start"], {
    detached: true,
    stdio: "ignore",
  });
  child.unref();
  console.log("[modelstat] ✓ background service restarted with new build");
}

/**
 * Kill any modelstat-related process the service supervisor didn't
 * reap. Two passes:
 *   (1) PID from ~/.modelstat/daemon.lock — the most reliable hit
 *       when the lock is fresh and our own daemon wrote it.
 *   (2) Process scan via `pgrep -f` on macOS/Linux — catches stray
 *       daemons launched by an OLDER bundle (different command
 *       line, different lock format), zombies that survived a
 *       launchd KeepAlive bounce, and the case where the user has
 *       BOTH the launchd-managed daemon AND a hand-spawned one
 *       running. The pattern is intentionally narrow
 *       (`modelstat\\.mjs`) so we don't kill `modelstatd` or any
 *       other process that happens to share a substring.
 *
 * SIGTERM first, escalate to SIGKILL after 2 s. All failures
 * tolerated — we just want the new bundle to be the only modelstat
 * process holding the lock when we restart.
 */
async function killStaleDaemon(stateDir) {
  await killByLockfile(stateDir);
  await killByProcessScan();
}

async function killByProcessScan() {
  const { spawnSync } = await import("node:child_process");
  // pgrep is on macOS by default, Linux via procps.
  const r = spawnSync("pgrep", ["-f", "modelstat\\.mjs"], {
    encoding: "utf8",
  });
  if (r.status !== 0 || !r.stdout) return;
  const pids = r.stdout
    .split(/\s+/)
    .map((s) => Number(s))
    .filter((n) => Number.isInteger(n) && n > 0 && n !== process.pid);
  if (pids.length === 0) return;
  for (const pid of pids) {
    try {
      process.kill(pid, "SIGTERM");
    } catch {
      /* gone or not ours */
    }
  }
  // Wait up to 2 s for graceful exits.
  for (let i = 0; i < 20; i++) {
    let alive = 0;
    for (const pid of pids) {
      try {
        process.kill(pid, 0);
        alive += 1;
      } catch {
        /* exited */
      }
    }
    if (alive === 0) return;
    await new Promise((r) => setTimeout(r, 100));
  }
  for (const pid of pids) {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      /* gone or denied */
    }
  }
}

async function killByLockfile(stateDir) {
  const { readFileSync } = await import("node:fs");
  const lockPath = join(stateDir, "daemon.lock");
  let payload;
  try {
    payload = JSON.parse(readFileSync(lockPath, "utf8"));
  } catch {
    return; // no lock file — nothing to do
  }
  const pid = Number(payload?.pid);
  if (!Number.isInteger(pid) || pid <= 0) return;
  if (pid === process.pid) return; // shouldn't happen, but defensive
  try {
    process.kill(pid, "SIGTERM");
  } catch (err) {
    if (err && err.code === "ESRCH") return; // already gone
    // EPERM: not our process — leave it alone.
    return;
  }
  // Wait up to 2 s for graceful exit.
  for (let i = 0; i < 20; i++) {
    try {
      process.kill(pid, 0); // signal 0 = existence check
    } catch {
      return; // exited
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  try {
    process.kill(pid, "SIGKILL");
  } catch {
    /* gone or denied */
  }
}

main().catch((err) => {
  console.warn(
    `[modelstat] postinstall failed: ${err && err.message ? err.message : err}`,
  );
  // Never fail the install for a postinstall problem.
  process.exit(0);
});
