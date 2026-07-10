/**
 * Install/uninstall the modelstat daemon as a background service.
 *
 * macOS → launchd user agent at ~/Library/LaunchAgents/ai.modelstat.daemon.plist
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
  readdirSync,
  readFileSync,
  realpathSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { homedir, platform, userInfo } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { state } from "./config.js";

export const SERVICE_LABEL = "ai.modelstat.daemon";
export const SYSTEMD_UNIT = "modelstat"; // → modelstat.service
// The tray gets its OWN launchd agent, separate from the daemon's, so the
// menu-bar icon is brought back on every login by launchd — not by the tray
// trying to register itself as a Login Item while it happens to be running
// (the old chicken-and-egg trap that left it dead after any reboot/crash).
export const TRAY_SERVICE_LABEL = "ai.modelstat.tray";

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

/** Pinned fallbacks if we can't read the version from the source tree (kept in
 * sync with each package's range in apps/daemon/package.json + daemon-core). */
const NODE_LLAMA_CPP_FALLBACK_VERSION = "3.18.1";
const HF_TRANSFORMERS_FALLBACK_VERSION = "3.8.1";

/** Read a package's installed version from the tree we're running out of, so the
 * staged self-contained copy matches exactly what this bundle was built and
 * tested against. package.json may be hidden behind an `exports` map, so resolve
 * the main entry and walk up to the owning directory. Returns null if it isn't
 * resolvable (e.g. we're running from the already-orphaned bundle). */
function sourcePkgVersion(sourceCli: string, pkgName: string): string | null {
  try {
    const req = createRequire(sourceCli);
    let d = dirname(realpathSync(req.resolve(pkgName)));
    for (let i = 0; i < 10; i++) {
      const pj = join(d, "package.json");
      if (existsSync(pj)) {
        const p = JSON.parse(readFileSync(pj, "utf8")) as {
          name?: string;
          version?: string;
        };
        if (p.name === pkgName && p.version) return p.version;
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

/** The version of a package already staged in `~/.modelstat/bin/node_modules`,
 * or null if it isn't there. */
function stagedVersion(pkgName: string): string | null {
  try {
    const pj = join(binDir(), "node_modules", ...pkgName.split("/"), "package.json");
    return (JSON.parse(readFileSync(pj, "utf8")) as { version?: string }).version ?? null;
  } catch {
    return null;
  }
}

/**
 * Stage native runtimes the dependency-free bundle imports into
 * `~/.modelstat/bin/node_modules/` with ONE `npm install` of every `name@version`
 * spec, so the bundle can `import()` them with nothing but npm in the picture —
 * no Ollama, no system libraries, no separate native build.
 *
 * CRITICAL — a SINGLE install, not one-per-package: these are installed with
 * `--no-save`, and `npm install` PRUNES extraneous packages. Two separate
 * `npm install --no-save` calls into the same prefix therefore remove the FIRST
 * package ("added 1, removed 1") — which silently deleted node-llama-cpp when
 * @huggingface/transformers was staged and dropped the daemon to the extractive
 * fallback. Installing every spec together keeps them all.
 *
 * Why npm and not a file copy: these ship large per-platform prebuilt binary
 * closures (node-llama-cpp's `@node-llama-cpp/<plat>`, transformers'
 * `onnxruntime-node`); `npm install` resolves the closure + the right platform
 * binary for us. Never throws — returns false on failure so the caller decides
 * what's load-bearing.
 */
function stageNativePkgs(specs: string[]): boolean {
  const dest = binDir();
  mkdirSync(dest, { recursive: true });
  // When this runs from the npm-UPGRADE path it's nested inside
  // `npm install -g modelstat`'s postinstall, so npm's own config leaks in as
  // `npm_config_*` env vars — notably `npm_config_global=true` and the global
  // `npm_config_prefix`. Left alone, the nested install would treat `--prefix`
  // as a GLOBAL root and drop packages in `<dest>/lib/node_modules` instead of
  // `<dest>/node_modules`, where the bundle's walk-up can't find them. Force a
  // LOCAL install (`--global=false`) and scrub the two leaking vars; keep the
  // rest (registry, cache, proxy, auth) so corporate mirrors still work.
  const childEnv = { ...process.env };
  delete childEnv.npm_config_global;
  delete childEnv.npm_config_prefix;
  // This step used to be a silent black box: seconds with a warm cache, but a
  // multi-minute (apparently-frozen) network download on a cold one. Announce it.
  process.stderr.write(`  · staging native runtime (${specs.join(", ")})…\n`);
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
      // Prefer the npm cache the current install already populated — an offline
      // ~3s copy instead of a redundant network re-fetch. Only a genuine cache
      // miss touches the network, and a capped per-request timeout makes that
      // fail fast (the daemon re-stages on its next preflight) rather than hang.
      "--prefer-offline",
      "--fetch-timeout=60000",
      "--loglevel=error",
      ...specs,
    ],
    { encoding: "utf8", stdio: "pipe", env: childEnv },
  );
  if (r.status !== 0) {
    process.stderr.write(
      `[modelstat] npm couldn't stage [${specs.join(", ")}] into ~/.modelstat/bin:\n${(r.stderr || r.stdout || "").trim()}\n`,
    );
    return false;
  }
  return true;
}

/**
 * Stage the native runtimes the bundle imports into `~/.modelstat/bin`. Runs
 * from inside `installBundle()` on every `connect` / service refresh, straight
 * out of the freshly-unpacked npm tree where npm is on PATH.
 *
 * @huggingface/transformers (the embedder + on-device PII/NER redactor) is
 * staged in EVERY mode — redaction/embeddings stay on-device regardless of
 * where summarisation happens. node-llama-cpp (the local summariser, ~big
 * native binary + the ~2.7 GB model it then pulls) is staged ONLY in `local`
 * mode: cloud and self-hosted summarise elsewhere, so skipping it is what keeps
 * them off the native-runtime footprint.
 *
 * When both are needed they go in ONE install (see stageNativePkgs — separate
 * `--no-save` installs prune each other). If a combined local-mode install
 * fails (e.g. transformers' onnxruntime binary), retry with just node-llama-cpp
 * so a transformers hiccup can never cost us the summariser. Best-effort: never
 * throws; skips the (network) install when every package is already staged.
 */
function installNativeRuntime(sourceCli: string): string[] {
  const local = state.summarizerMode === "local";
  const transformers = {
    name: "@huggingface/transformers",
    version:
      sourcePkgVersion(sourceCli, "@huggingface/transformers") ?? HF_TRANSFORMERS_FALLBACK_VERSION,
  };
  const llama = {
    name: "node-llama-cpp",
    version: sourcePkgVersion(sourceCli, "node-llama-cpp") ?? NODE_LLAMA_CPP_FALLBACK_VERSION,
  };
  // node-llama-cpp only in local mode; transformers always.
  const pkgs = local ? [llama, transformers] : [transformers];
  // Every package already staged at the right version → nothing to do.
  if (pkgs.every((p) => stagedVersion(p.name) === p.version)) {
    return pkgs.map((p) => `${p.name}@${p.version} (cached)`);
  }
  const specs = pkgs.map((p) => `${p.name}@${p.version}`);
  if (stageNativePkgs(specs)) return specs;
  if (local) {
    // Combined install failed — ensure at least the CRITICAL summariser runtime
    // is staged (the embedder/redactor degrades to server-side embed + regex/LLM).
    process.stderr.write(
      "[modelstat] retrying with just the summariser runtime; the embedder/redactor will fall back…\n",
    );
    const llamaSpec = `node-llama-cpp@${llama.version}`;
    if (stageNativePkgs([llamaSpec])) return [llamaSpec];
    process.stderr.write(
      `[modelstat] couldn't stage the summariser runtime (${llamaSpec}); the daemon uses the extractive fallback until this is resolved.\n`,
    );
  } else {
    // Cloud/self-hosted: no local summariser to fall back on. A transformers
    // miss just degrades on-device NER/embeddings (cloud fail-closes to local
    // extractive; self-hosted keeps its remote model).
    process.stderr.write(
      "[modelstat] couldn't stage @huggingface/transformers; on-device NER/embeddings degrade until this is resolved.\n",
    );
  }
  return [];
}

/** Best-effort: absolute path to the node binary we'd invoke. */
function nodeBinary(): string {
  return process.execPath;
}

/**
 * Remove the retired "modelstat agent" launcher (a rename of node) and any
 * libnode staged beside it, left behind by older installs.
 *
 * We no longer relocate node to give the daemon a pretty Activity-Monitor name.
 * Homebrew's `node` is a thin stub that loads its engine from a separate
 * `libnode.<v>.dylib` sitting next to it, so relocating the stub orphaned that
 * library — and a self-update that wiped the orphan bricked the daemon
 * (`dyld: Library not loaded: @rpath/libnode.<v>.dylib`). The daemon now runs
 * through node IN PLACE (what node is built for), which can't break that way.
 * `process.title` (set in cli.ts cmdStart) still labels it "modelstat agent" in
 * `ps`/`top` and on Linux; on macOS Activity Monitor it shows as "node" — the
 * accepted cosmetic cost of not relocating a runtime.
 *
 * Runs on install so an upgrade actively clears the old artifacts (including a
 * hand-made libnode symlink) instead of leaving dead weight. Best-effort; never
 * throws. macOS-only — nothing else creates these.
 */
export function cleanupStaleLauncher(): void {
  if (platform() !== "darwin") return;
  const bin = binDir();
  try {
    const legacy = join(bin, "modelstat agent");
    if (existsSync(legacy)) unlinkSync(legacy);
  } catch {
    /* harmless if it lingers — nothing executes it anymore */
  }
  try {
    for (const f of readdirSync(bin)) {
      if (/^libnode\..*\.dylib$/.test(f)) unlinkSync(join(bin, f));
    }
  } catch {
    /* bin dir absent, or a file vanished mid-scan — nothing to clean */
  }
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

/**
 * Pure launchd plist body for the daemon agent: `nodeBin` runs the bundled
 * CLI's `start`. Split out from writePlist (the filesystem side effects) so the
 * launch contract is unit-testable. We exec node directly, in place — not a
 * relocated/renamed copy; see cleanupStaleLauncher for the why.
 */
export function daemonPlistContents(nodeBin: string, cliPath: string): string {
  const programArgs = [
    `    <string>${nodeBin}</string>`,
    `    <string>${cliPath}</string>`,
    `    <string>start</string>`,
  ].join("\n");
  return `<?xml version="1.0" encoding="UTF-8"?>
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
}

function writePlist(cliPath: string): string {
  const p = plistPath();
  mkdirSync(dirname(p), { recursive: true });
  // This agent runs the headless daemon directly. The menu-bar tray has its
  // OWN launchd agent (installTrayAutostart) — keeping them separate means
  // the pipeline stays up even if the GUI tray is quit, and each is
  // supervised independently. The daemon needs no GUI and self-heals via
  // RunAtLoad + KeepAlive. It execs node IN PLACE (process.execPath), not a
  // renamed copy — process.title covers `ps`/`top`; see cleanupStaleLauncher.
  const plist = daemonPlistContents(nodeBinary(), cliPath);
  writeFileSync(p, plist, { mode: 0o644 });
  return p;
}

function launchctl(args: string[]): { ok: boolean; out: string; err: string } {
  const r = spawnSync("launchctl", args, { encoding: "utf8" });
  return { ok: r.status === 0, out: r.stdout ?? "", err: r.stderr ?? "" };
}

function macInstall(): void {
  const cliPath = installBundle();
  // Clear the retired "modelstat agent" launcher + any orphaned libnode from
  // older installs; the daemon now execs node in place.
  cleanupStaleLauncher();
  const plist = writePlist(cliPath);
  const uid = userInfo().uid;
  const target = `gui/${uid}/${SERVICE_LABEL}`;
  // Idempotent: unload the previous instance, then load + start the fresh one.
  launchctl(["bootout", target]);
  const boot = launchctl(["bootstrap", `gui/${uid}`, plist]);
  if (!boot.ok) {
    throw new Error(`launchctl bootstrap failed: ${boot.err.trim()}`);
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
Description=modelstat daemon
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
    `Service installation isn't supported on ${p}. Run 'modelstat start' manually to keep the daemon running.`,
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
 * Copy a built ModelstatTray.app bundle to ~/Applications. The daemon's
 * launchd agent runs the headless daemon; the tray gets its OWN launchd
 * agent (installTrayAutostart) that execs this bundle's binary. Used by
 * `npx modelstat@latest` on macOS when a source .app is available (the
 * npm package ships one, and the installer build-compiles one if Swift
 * is on $PATH).
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

/* ─── macOS tray autostart (its own launchd agent) ────────────────────
 *
 * The tray gets a launchd user agent SEPARATE from the daemon's. Unlike
 * the daemon agent (which runs a headless `node … start`), this one execs
 * the tray's GUI binary directly — which works from a user agent in the
 * gui/<uid> domain: the menu-bar icon comes up and launchd supervises it.
 *
 * KeepAlive={SuccessfulExit:false} is the whole trick for telling a crash
 * apart from the user quitting, with no marker files:
 *   · user picks Quit       → NSApp.terminate exits 0 → launchd leaves it
 *                             dead (no instant-relaunch fight); it returns
 *                             on the next login via RunAtLoad.
 *   · crash (non-zero/signal) → launchd relaunches it.
 *   · login-race duplicate   → the tray's process-table single-instance
 *                             guard exits 0 → treated as a clean exit, so
 *                             launchd doesn't fight it either.
 * This mirrors the daemon agent's KeepAlive (see writePlist).
 */

export function trayPlistPath(): string {
  return join(home(), "Library", "LaunchAgents", `${TRAY_SERVICE_LABEL}.plist`);
}

/**
 * Render the tray agent's launchd plist XML. Pure (no I/O) and exported so
 * the load-bearing contract is unit-testable without invoking launchctl:
 * the agent execs THIS binary, RunAtLoad brings it back at login, and
 * KeepAlive={SuccessfulExit:false} restarts a crash but not a clean quit.
 */
export function trayPlistContents(trayBinary: string): string {
  // The tray shells out to `/usr/bin/env node …` to run the CLI. launchd
  // agents start with a bare PATH (/usr/bin:/bin:/usr/sbin:/sbin), so unless
  // we extend it `node` isn't found and the tray hangs on "Loading…". Put the
  // EXACT node that ran this installer first (process.execPath's dir) — the
  // same node the daemon agent is pinned to — so it resolves regardless of
  // where node lives (nvm, keg-only Homebrew, /usr/local). Keep the common
  // Homebrew/local bins as fallbacks.
  const nodeDir = dirname(nodeBinary());
  const trayPath = `${nodeDir}:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin`;
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>${TRAY_SERVICE_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${trayBinary}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict><key>SuccessfulExit</key><false/></dict>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>${join(logDir(), "tray-out.log")}</string>
  <key>StandardErrorPath</key><string>${join(logDir(), "tray-err.log")}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>${trayPath}</string>
  </dict>
  <key>WorkingDirectory</key><string>${home()}</string>
</dict>
</plist>
`;
}

function writeTrayPlist(trayBinary: string): string {
  const p = trayPlistPath();
  mkdirSync(dirname(p), { recursive: true });
  writeFileSync(p, trayPlistContents(trayBinary), { mode: 0o644 });
  return p;
}

/** Kill any running tray process so the launchd agent's own instance
 *  becomes the sole (and supervised) one — e.g. if a copy was launched by
 *  hand from Finder. Matches the full bundle path to avoid unrelated
 *  processes. */
function killStrayTray(): void {
  spawnSync("pkill", ["-f", "ModelstatTray.app/Contents/MacOS/modelstat-tray"]);
}

/**
 * Install the tray's launchd agent so the menu-bar icon starts at login,
 * comes back after a reboot, and is restarted if it crashes — and start it
 * immediately. Best-effort: returns the plist path on success, or null on
 * non-macOS / when no tray binary is installed (callers degrade to the
 * headless daemon path). Mirrors macInstall().
 *
 * We kill any pre-existing tray process first so the agent's own kickstart
 * instance wins the single-instance race and is the one launchd supervises.
 */
export function installTrayAutostart(): { path: string } | null {
  if (platform() !== "darwin") return null;
  const trayBinary = locateTrayExecutable();
  if (!trayBinary) return null;
  mkdirSync(logDir(), { recursive: true });
  const plist = writeTrayPlist(trayBinary);
  const uid = userInfo().uid;
  const target = `gui/${uid}/${TRAY_SERVICE_LABEL}`;
  killStrayTray();
  // Idempotent: drop any previous instance, then load + start.
  launchctl(["bootout", target]);
  launchctl(["bootstrap", `gui/${uid}`, plist]);
  launchctl(["kickstart", "-k", target]);
  return { path: plist };
}

/**
 * Remove the tray's launchd agent and stop the tray. Boots the agent out
 * (which stops the supervised instance), deletes the plist, and kills any
 * stray tray process so `npx modelstat remove` actually makes the icon go
 * away. No-op on non-macOS.
 */
export function uninstallTrayAutostart(): void {
  if (platform() !== "darwin") return;
  const uid = userInfo().uid;
  launchctl(["bootout", `gui/${uid}/${TRAY_SERVICE_LABEL}`]);
  const plist = trayPlistPath();
  if (existsSync(plist)) {
    try {
      unlinkSync(plist);
    } catch {
      /* ignore */
    }
  }
  killStrayTray();
}

/**
 * Remove the installed tray .app bundle from ~/Applications. Called on
 * uninstall so nothing is left behind. No-op on non-macOS or if absent.
 */
export function removeTrayApp(): void {
  if (platform() !== "darwin") return;
  const dest = join(home(), "Applications", "ModelstatTray.app");
  if (existsSync(dest)) spawnSync("rm", ["-rf", dest]);
}

/** True if a tray .app is currently installed in ~/Applications. */
export function trayInstalled(): boolean {
  return locateTrayExecutable() !== null;
}

/**
 * On upgrade, refresh an ALREADY-installed tray to the bundle shipped with
 * THIS package and re-arm its autostart agent, so the NEW tray version ends
 * up running. Called from `_install-service` (the auto-update / npm-update
 * refresh step). Only touches things if a tray was already installed — it
 * won't add a tray to a machine that never had one — and only uses a
 * PREBUILT bundle, so a background upgrade never blocks on a Swift compile.
 * No-op on non-macOS. Best-effort: returns true if it refreshed + re-armed.
 */
export function refreshTrayIfInstalled(): boolean {
  if (platform() !== "darwin") return false;
  if (!trayInstalled()) return false;
  const src = prebuiltTrayAppPath();
  if (!src) return false;
  installTrayApp(src);
  return installTrayAutostart() !== null;
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
/** The already-built `.app` candidates (no compile): the CI-bundled
 *  vendor copy, then the local dev build output. Returns the first that
 *  exists, or null. Split out so the upgrade refresh can find a prebuilt
 *  bundle WITHOUT ever triggering a `swift build`. */
function prebuiltTrayAppPath(): string | null {
  const here = dirname(fileURLToPath(import.meta.url));
  const candidates = [
    // Pre-built .app — CI with codesigning drops one here.
    join(here, "..", "vendor", "ModelstatTray.app"),
    // Local dev layout: apps/daemon/src/service.ts → ../../tray-mac/build/ModelstatTray.app
    join(here, "..", "..", "tray-mac", "build", "ModelstatTray.app"),
  ];
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  return null;
}

export async function bundledTrayAppPath(progress?: TrayBuildProgress): Promise<string | null> {
  if (platform() !== "darwin") return null;
  const prebuilt = prebuiltTrayAppPath();
  if (prebuilt) return prebuilt;
  const here = dirname(fileURLToPath(import.meta.url));
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
