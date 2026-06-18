/**
 * Install/uninstall the modelstat agent as a background service.
 *
 * macOS → launchd user agent at ~/Library/LaunchAgents/ai.modelstat.agent.plist
 * Linux → systemd --user unit at ~/.config/systemd/user/modelstat.service
 *
 * We copy the running CLI bundle to a stable path (~/.modelstat/bin) so
 * the service survives npx cache cleanups and `npm uninstall -g`.
 * Logs go to ~/.modelstat/logs/{out,err}.log. launchd/systemd don't
 * rotate these; the daemon truncates any log over LOG_MAX_BYTES at
 * boot (tail preserved in <name>.old.log) — see rotateRunawayLogs in
 * daemon.ts.
 */
import { spawn, spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { homedir, platform, userInfo } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export const SERVICE_LABEL = "ai.modelstat.agent";
export const SYSTEMD_UNIT = "modelstat"; // → modelstat.service

function home(): string {
  return homedir();
}
function stateDir(): string {
  return join(home(), ".modelstat");
}
function binDir(): string {
  return join(stateDir(), "bin");
}
function logDir(): string {
  return join(stateDir(), "logs");
}
function installedCliPath(): string {
  return join(binDir(), "modelstat.mjs");
}

/** Locate the currently-running CLI script. */
function runningCliPath(): string {
  // When tsup-bundled, import.meta.url is the dist/cli.mjs. When running
  // via tsx in dev, it's the TypeScript source. Either way, this is the
  // file to copy.
  return fileURLToPath(import.meta.url).replace(/service\.(mjs|js|ts)$/, "cli.mjs");
}

/** Copy the bundle to ~/.modelstat/bin so the service has a stable target. */
function installBundle(): string {
  mkdirSync(binDir(), { recursive: true });
  mkdirSync(logDir(), { recursive: true });
  const src = runningCliPath();
  const dest = installedCliPath();
  if (!existsSync(src)) {
    throw new Error(
      `Can't find the CLI bundle to install from (${src}). Are you running a local dev build?`,
    );
  }
  copyFileSync(src, dest);
  // Lay the native summariser runtime down beside the bundle so the
  // copied, dependency-free `~/.modelstat/bin/modelstat.mjs` can load
  // it with nothing but npm in the picture — no Ollama, no system
  // libs, no separate build step. Sourced from the package we're
  // running from RIGHT NOW (connect/upgrade always run straight out of
  // the npm install that has these deps), so it can't miss the way the
  // postinstall hook does for npx-prewarm / manual-copy installs.
  installNativeRuntime(src);
  return dest;
}

/** Public, side-effect-only entry point for `modelstat _setup-runtime`
 * — copies the bundle + stages the native summariser runtime without
 * touching launchd/systemd or the user's identity. Lets the install
 * pipeline (and tests) wire a self-contained `~/.modelstat/bin` in one
 * step. Returns the installed bundle path. */
export function setupRuntime(): string {
  return installBundle();
}

/** Pinned fallback if we can't read the version from the source tree
 * (kept in sync with apps/agent-dev/package.json's node-llama-cpp range). */
const NODE_LLAMA_CPP_FALLBACK_VERSION = "3.18.1";

/** Read node-llama-cpp's installed version from the package we're
 * running out of, so the staged self-contained copy matches exactly
 * what this bundle was built and tested against. Its package.json is
 * hidden behind an `exports` map, so resolve the main entry and walk
 * up to the owning directory. Returns null if it isn't resolvable
 * (e.g. we're running from the already-orphaned bundle). */
function sourceLlamaVersion(sourceCli: string): string | null {
  try {
    const req = createRequire(sourceCli);
    let d = dirname(realpathSync(req.resolve("node-llama-cpp")));
    for (let i = 0; i < 10; i++) {
      const pj = join(d, "package.json");
      if (existsSync(pj)) {
        const p = JSON.parse(readFileSync(pj, "utf8")) as {
          name?: string;
          version?: string;
        };
        if (p.name === "node-llama-cpp" && p.version) return p.version;
      }
      const up = dirname(d);
      if (up === d) break;
      d = up;
    }
  } catch {
    /* not resolvable from here */
  }
  return null;
}

/**
 * Lay a COMPLETE, self-contained `node-llama-cpp` runtime down beside
 * the installed bundle (`~/.modelstat/bin/node_modules/`) so the
 * dependency-free `modelstat.mjs` can `import("node-llama-cpp")` at
 * runtime with nothing but npm in the picture — no Ollama, no system
 * libraries, no separate native build.
 *
 * Why npm and not a file copy: node-llama-cpp ships ~30 runtime
 * dependencies plus a per-platform prebuilt-binary sibling
 * (`@node-llama-cpp/<plat>`). A hand-rolled copy would have to
 * replicate that whole closure correctly across npm-flat AND pnpm's
 * symlinked store — exactly the kind of brittle wiring that left this
 * machine without a summariser. `npm install` resolves the closure and
 * the right platform binary for us. This is the single place we shell
 * out to npm; everything else the agent needs is inlined in the bundle.
 *
 * Runs from inside `installBundle()`, i.e. on every `connect` / service
 * refresh — straight out of the freshly-unpacked npm tree, where npm is
 * on PATH — so it can't be skipped the way the postinstall hook is for
 * npx-prewarm / manual-copy installs.
 *
 * Best-effort: never throws. Skips the (network) install when the
 * matching version is already staged, so re-running `connect` is fast.
 */
function installNativeRuntime(sourceCli: string): string[] {
  const version = sourceLlamaVersion(sourceCli) ?? NODE_LLAMA_CPP_FALLBACK_VERSION;
  const dest = binDir();
  // Already staged at the right version → nothing to do.
  try {
    const have = JSON.parse(
      readFileSync(join(dest, "node_modules", "node-llama-cpp", "package.json"), "utf8"),
    ) as { version?: string };
    if (have.version === version) return [`node-llama-cpp@${version} (cached)`];
  } catch {
    /* not staged yet — fall through and install */
  }

  mkdirSync(dest, { recursive: true });
  // When this runs from the npm-UPGRADE path it's nested inside
  // `npm install -g modelstat`'s postinstall, so npm's own config leaks
  // in as `npm_config_*` env vars — notably `npm_config_global=true` and
  // the global `npm_config_prefix`. Left alone, the nested install would
  // treat `--prefix` as a GLOBAL root and drop the package in
  // `<dest>/lib/node_modules` instead of `<dest>/node_modules`, where
  // the bundle's `node_modules` walk-up can't find it. Force a LOCAL
  // install (`--global=false`) and scrub the two leaking vars; keep the
  // rest (registry, cache, proxy, auth) so corporate mirrors still work.
  const childEnv = { ...process.env };
  delete childEnv.npm_config_global;
  delete childEnv.npm_config_prefix;
  // This step used to be a silent black box: a few seconds with a warm cache,
  // but a multi-minute (apparently-frozen) network download on a cold/slow one —
  // `connect` looked hung right here. Announce it so it isn't a mystery.
  process.stderr.write(`  · staging summariser runtime (node-llama-cpp@${version})…\n`);
  const r = spawnSync(
    "npm",
    [
      "install",
      "--prefix",
      dest,
      "--global=false",
      "--no-save",
      "--omit=dev",
      "--no-audit",
      "--no-fund",
      // Prefer the npm cache the current install already populated (node-llama-cpp
      // is a direct dep, so the platform prebuilt is cached) — an offline ~3s copy
      // instead of a redundant network re-fetch. Only a genuine cache miss touches
      // the network, and a capped per-request timeout makes that fail fast (the
      // daemon's summariser preflight re-stages) rather than hang indefinitely.
      "--prefer-offline",
      "--fetch-timeout=60000",
      "--loglevel=error",
      `node-llama-cpp@${version}`,
    ],
    { encoding: "utf8", stdio: "pipe", env: childEnv },
  );
  if (r.status !== 0) {
    // Don't abort the install — the daemon's preflight is the backstop
    // and prints an actionable message if the summariser truly can't
    // load. Surface the npm error so `connect` output isn't silent.
    process.stderr.write(
      `[modelstat] couldn't stage the bundled summariser runtime via npm` +
        ` (node-llama-cpp@${version}); the daemon's summariser preflight will fail until this is resolved.\n` +
        `${(r.stderr || r.stdout || "").trim()}\n`,
    );
    return [];
  }
  return [`node-llama-cpp@${version}`];
}

/** Best-effort: absolute path to the node binary we'd invoke. */
function nodeBinary(): string {
  return process.execPath;
}

/* ─── macOS ────────────────────────────────────────────────────────── */

function plistPath(): string {
  return join(home(), "Library", "LaunchAgents", `${SERVICE_LABEL}.plist`);
}

/**
 * Look for the menu-bar tray app. Returns the path to the launchable
 * binary inside the bundle, or null if the tray isn't installed. We
 * check ~/Applications first (user-local install, what we write in
 * installTrayApp()), then /Applications (system-wide DMG drag).
 * If found, the launchd plist launches the tray, which in turn spawns
 * `modelstat start` as a subprocess so the user gets the menu-bar
 * status icon AND the pipeline in one managed process.
 */
function locateTrayExecutable(): string | null {
  const candidates = [
    join(home(), "Applications", "ModelstatTray.app", "Contents", "MacOS", "modelstat-tray"),
    "/Applications/ModelstatTray.app/Contents/MacOS/modelstat-tray",
  ];
  for (const p of candidates) {
    if (existsSync(p)) return p;
  }
  return null;
}

function writePlist(cliPath: string): string {
  const p = plistPath();
  mkdirSync(dirname(p), { recursive: true });
  // Run the daemon directly, NOT the menu-bar tray: the tray GUI app exits
  // 78 (EX_CONFIG) under launchd — AppKit can't come up in that context —
  // before it ever spawns `modelstat start`, which left the device
  // permanently disconnected. The daemon needs no GUI and self-heals via
  // RunAtLoad + KeepAlive. The tray bundle is still staged (installTrayApp)
  // for anyone who wants to launch it by hand.
  const programArgs = [
    `    <string>${nodeBinary()}</string>`,
    `    <string>${cliPath}</string>`,
    `    <string>start</string>`,
  ].join("\n");
  const plist = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>${SERVICE_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
${programArgs}
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>${join(logDir(), "out.log")}</string>
  <key>StandardErrorPath</key><string>${join(logDir(), "err.log")}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
    <!-- Heap headroom for the startup scan of a large transcript backlog.
         Node's default old-space ceiling (~4 GB) OOM-crashed the daemon on
         big histories; raise it well below typical RAM. -->
    <key>NODE_OPTIONS</key><string>--max-old-space-size=8192</string>
  </dict>
  <key>WorkingDirectory</key><string>${home()}</string>
</dict>
</plist>
`;
  writeFileSync(p, plist, { mode: 0o644 });
  return p;
}

function launchctl(args: string[]): { ok: boolean; out: string; err: string } {
  const r = spawnSync("launchctl", args, { encoding: "utf8" });
  return { ok: r.status === 0, out: r.stdout ?? "", err: r.stderr ?? "" };
}

function macInstall(): void {
  const cliPath = installBundle();
  const plist = writePlist(cliPath);
  const uid = userInfo().uid;
  const target = `gui/${uid}/${SERVICE_LABEL}`;
  // Idempotent: unload the previous instance if there is one.
  launchctl(["bootout", target]);
  const boot = launchctl(["bootstrap", `gui/${uid}`, plist]);
  if (!boot.ok && !/already loaded|service already bootstrapped/i.test(boot.err)) {
    // bootstrap can fail on older macOS; fall back to load.
    const load = launchctl(["load", "-w", plist]);
    if (!load.ok) {
      throw new Error(
        `launchctl load failed:\n  bootstrap: ${boot.err.trim()}\n  load: ${load.err.trim()}`,
      );
    }
  }
  launchctl(["kickstart", "-k", target]);
}

function macUninstall(): void {
  const uid = userInfo().uid;
  const target = `gui/${uid}/${SERVICE_LABEL}`;
  launchctl(["bootout", target]);
  const plist = plistPath();
  if (existsSync(plist)) {
    try {
      unlinkSync(plist);
    } catch {
      /* ignore */
    }
  }
}

function macStatus(): { running: boolean; hint: string } {
  const uid = userInfo().uid;
  const r = launchctl(["print", `gui/${uid}/${SERVICE_LABEL}`]);
  return { running: r.ok, hint: r.ok ? "launchd managed" : "not installed" };
}

/* ─── Linux (systemd --user) ───────────────────────────────────────── */

function systemdUnitPath(): string {
  const xdg = process.env.XDG_CONFIG_HOME ?? join(home(), ".config");
  return join(xdg, "systemd", "user", `${SYSTEMD_UNIT}.service`);
}

function writeSystemdUnit(cliPath: string): string {
  const unitPath = systemdUnitPath();
  mkdirSync(dirname(unitPath), { recursive: true });
  const unit = `[Unit]
Description=modelstat agent
Documentation=https://modelstat.ai
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
# Heap headroom for the startup scan of a large transcript backlog —
# Node's default ~4 GB old-space ceiling OOM-crashed the daemon on big
# histories.
Environment=NODE_OPTIONS=--max-old-space-size=8192
ExecStart=${nodeBinary()} ${cliPath} start
Restart=always
RestartSec=10
# Don't restart-storm if the service is persistently unreachable.
StartLimitIntervalSec=300
StartLimitBurst=10
StandardOutput=append:${join(logDir(), "out.log")}
StandardError=append:${join(logDir(), "err.log")}

[Install]
WantedBy=default.target
`;
  writeFileSync(unitPath, unit, { mode: 0o644 });
  return unitPath;
}

function systemctl(args: string[]): { ok: boolean; out: string; err: string } {
  const r = spawnSync("systemctl", ["--user", ...args], { encoding: "utf8" });
  return { ok: r.status === 0, out: r.stdout ?? "", err: r.stderr ?? "" };
}

function linuxInstall(): void {
  const cliPath = installBundle();
  writeSystemdUnit(cliPath);
  // Reload so systemd picks up changes on reinstall.
  systemctl(["daemon-reload"]);
  const en = systemctl(["enable", "--now", `${SYSTEMD_UNIT}.service`]);
  if (!en.ok) {
    throw new Error(`systemctl enable failed: ${en.err.trim()}`);
  }
  // If already running, restart so it picks up the fresh bundle.
  systemctl(["restart", `${SYSTEMD_UNIT}.service`]);
}

function linuxUninstall(): void {
  systemctl(["disable", "--now", `${SYSTEMD_UNIT}.service`]);
  const unit = systemdUnitPath();
  if (existsSync(unit)) {
    try {
      unlinkSync(unit);
    } catch {
      /* ignore */
    }
  }
  systemctl(["daemon-reload"]);
}

function linuxStatus(): { running: boolean; hint: string } {
  const r = systemctl(["is-active", `${SYSTEMD_UNIT}.service`]);
  const active = r.out.trim() === "active";
  return { running: active, hint: active ? "systemd managed" : "not running" };
}

/* ─── Public API ──────────────────────────────────────────────────── */

export function installService(): { path: string; logs: string } {
  const p = platform();
  if (p === "darwin") {
    macInstall();
    return { path: plistPath(), logs: logDir() };
  }
  if (p === "linux") {
    linuxInstall();
    return { path: systemdUnitPath(), logs: logDir() };
  }
  throw new Error(
    `Service installation isn't supported on ${p}. Run 'modelstat start' manually to keep the agent running.`,
  );
}

export function uninstallService(): void {
  const p = platform();
  if (p === "darwin") return macUninstall();
  if (p === "linux") return linuxUninstall();
  throw new Error(`Service uninstall isn't supported on ${p}.`);
}

export function serviceStatus(): { running: boolean; hint: string } {
  const p = platform();
  if (p === "darwin") return macStatus();
  if (p === "linux") return linuxStatus();
  return { running: false, hint: `unsupported platform (${p})` };
}

export function logsDir(): string {
  return logDir();
}

export function absoluteBundlePath(): string {
  return installedCliPath();
}

/**
 * Copy a built ModelstatTray.app bundle to ~/Applications so the
 * launchd plist can pick it up. Used by `npx modelstat@latest` on macOS
 * when a source .app is available (the npm package ships one, and the
 * installer script build-compiles one if Swift is on $PATH).
 *
 * The copy is rsync-like (subprocess: `cp -R`). We blow away any prior
 * install so a stale build doesn't silently linger — the bundle is
 * tiny (<2 MB) and idempotent install is worth more than a few ms.
 *
 * Returns { installedAt } on success, null if the source doesn't
 * exist so callers can degrade to the headless daemon path.
 */
export function installTrayApp(sourceAppPath: string): { installedAt: string } | null {
  if (platform() !== "darwin") return null;
  if (!existsSync(sourceAppPath)) return null;
  const dest = join(home(), "Applications", "ModelstatTray.app");
  mkdirSync(dirname(dest), { recursive: true });
  // Remove any prior install so we don't leave stale Info.plist / binary
  // combinations. `rm -rf` via spawnSync keeps the implementation off
  // the async fs API for simplicity — the CLI is short-lived.
  spawnSync("rm", ["-rf", dest]);
  const r = spawnSync("cp", ["-R", sourceAppPath, dest], { encoding: "utf8" });
  if (r.status !== 0) {
    throw new Error(`cp ModelstatTray.app failed: ${r.stderr?.trim() || `exit ${r.status}`}`);
  }
  // Guarantee the inner binary is executable. `pnpm pack` normalises file
  // modes in the published tarball and only keeps the exec bit on declared
  // `bin` entries, so the prebuilt vendor/ModelstatTray.app binary ships as
  // -rw-r--r-- — and `cp -R` faithfully copies that. launchd then fails to
  // exec it and quits with EX_CONFIG (78). chmod here makes the install
  // correct regardless of which path produced the bundle (prebuilt tarball
  // or on-device `build-app.sh`, which already chmod +x's its output).
  chmodSync(join(dest, "Contents", "MacOS", "modelstat-tray"), 0o755);
  return { installedAt: dest };
}

/** Progress sink for an on-device tray build. `onLine` receives each
 *  complete line of `build-app.sh` / SwiftPM output as it streams, so
 *  the caller can surface granular progress (a cold `swift build` of the
 *  tray is ~1 min and would otherwise look like a frozen terminal). */
export interface TrayBuildProgress {
  onLine?: (line: string) => void;
}

/**
 * Resolve a built-or-buildable ModelstatTray.app bundled with this CLI
 * package. Strategy (in order):
 *   1. Any pre-built `.app` sitting at the usual candidate paths —
 *      CI publishes one when codesigning is configured. This returns
 *      instantly and never invokes the progress sink.
 *   2. Swift sources shipped in `vendor/tray-mac/` (the default for
 *      the npm tarball); if found and `swift` is on $PATH, we invoke
 *      `build-app.sh` to produce the bundle on the user's machine,
 *      streaming compiler output line-by-line to `progress.onLine`.
 *
 * Async because path (2) is a cold compile that blocks for ~30–60s; we
 * stream it rather than buffering so onboarding shows live progress.
 * Returns null if we can't produce a bundle — callers degrade to the
 * headless launchd path.
 */
export async function bundledTrayAppPath(progress?: TrayBuildProgress): Promise<string | null> {
  if (platform() !== "darwin") return null;
  const here = dirname(fileURLToPath(import.meta.url));
  const candidates = [
    // Pre-built .app — CI with codesigning drops one here.
    join(here, "..", "vendor", "ModelstatTray.app"),
    // Local dev layout: apps/agent-dev/src/service.ts → ../../tray-mac/build/ModelstatTray.app
    join(here, "..", "..", "tray-mac", "build", "ModelstatTray.app"),
  ];
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  // Sources path — build locally if we can.
  const sourceDirs = [join(here, "..", "vendor", "tray-mac"), join(here, "..", "..", "tray-mac")];
  for (const src of sourceDirs) {
    const build = join(src, "build-app.sh");
    if (!existsSync(build)) continue;
    if (!hasSwift()) return null;
    const code = await runTrayBuild(src, build, progress);
    if (code === 0) {
      const app = join(src, "build", "ModelstatTray.app");
      if (existsSync(app)) return app;
    }
    // Fall through to try the next candidate; failures surface as the
    // streamed compiler output the caller already printed.
  }
  return null;
}

/**
 * Stateful line-splitter: feed it arbitrary stdout/stderr chunks and it
 * calls `onLine` once per complete line, regardless of how the OS
 * happened to slice the stream into chunks. `flush()` emits any trailing
 * partial line (e.g. a final "Build complete!" with no newline).
 * Exported so the streaming contract is unit-tested without spawning swift.
 */
export function createLineSplitter(onLine: (line: string) => void): {
  push: (chunk: string) => void;
  flush: () => void;
} {
  let buf = "";
  return {
    push(chunk: string): void {
      buf += chunk;
      for (;;) {
        const nl = buf.indexOf("\n");
        if (nl === -1) break;
        const line = buf.slice(0, nl).trimEnd();
        buf = buf.slice(nl + 1);
        if (line) onLine(line);
      }
    },
    flush(): void {
      const line = buf.trim();
      buf = "";
      if (line) onLine(line);
    },
  };
}

/** Spawn build-app.sh, streaming merged stdout+stderr to `progress.onLine`
 *  line-by-line. Resolves with the child's exit code (null on spawn error). */
function runTrayBuild(
  cwd: string,
  buildScript: string,
  progress?: TrayBuildProgress,
): Promise<number | null> {
  return new Promise((resolve) => {
    const child = spawn("bash", [buildScript], { cwd });
    const splitter = createLineSplitter((line) => progress?.onLine?.(line));
    const pump = (chunk: Buffer): void => splitter.push(chunk.toString("utf8"));
    child.stdout?.on("data", pump);
    child.stderr?.on("data", pump);
    child.on("error", () => resolve(null));
    child.on("close", (code) => {
      splitter.flush();
      resolve(code);
    });
  });
}

function hasSwift(): boolean {
  const r = spawnSync("swift", ["--version"], { encoding: "utf8" });
  return r.status === 0;
}

export function trayStatus(): { installed: boolean; path: string | null } {
  if (platform() !== "darwin") return { installed: false, path: null };
  const exe = locateTrayExecutable();
  return exe
    ? { installed: true, path: exe.replace(/\/Contents\/MacOS\/modelstat-tray$/, "") }
    : { installed: false, path: null };
}
