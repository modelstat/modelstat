/**
 * modelstat CLI: `connect`, `discover`, `scan`, `watch`, `stats`, `jobs`.
 *
 * The bundled build (tsup → dist/cli.cjs) adds a `#!/usr/bin/env node`
 * shebang at pack time — keep this source shebang-free so we don't emit
 * two of them.
 */
import { spawn } from "node:child_process";
import { arch as cpuArch, hostname, platform, release } from "node:os";
import { createInterface } from "node:readline";
import { discover } from "@modelstat/parsers";
import {
  DeviceMeUnauthorized,
  fetchDeviceMe,
  fetchDeviceViewByClaim,
  fetchDeviceViewJobsByClaim,
  fetchDeviceViewLedgerByClaim,
  reportDiscovery,
  selfRegister,
} from "./api.js";
import { state } from "./config.js";
import { backupIdentity, hasIdentityFile, identityPath } from "./identity.js";
import { deviceUuidFromMachineKey, machineKey, machineKeySource } from "./machine-key.js";
import { scanAll } from "./scan.js";
import {
  bundledTrayAppPath,
  installService,
  installTrayApp,
  logsDir,
  serviceStatus,
  setupRuntime,
  trayStatus,
  uninstallService,
} from "./service.js";
import { daemonHealth } from "./supervise.js";

/** Yes/no prompt over stdin. Returns `defaultYes` if stdin isn't a
 * TTY or the user just hits Enter. Lowercased "y"/"yes" counts as
 * yes, "n"/"no" as no; anything else falls back to `defaultYes`. */
async function confirmPrompt(question: string, defaultYes: boolean): Promise<boolean> {
  if (process.stdin.isTTY !== true) return defaultYes;
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  try {
    const raw: string = await new Promise((resolve) => rl.question(question, resolve));
    const ans = raw.trim().toLowerCase();
    if (ans === "") return defaultYes;
    if (ans === "y" || ans === "yes") return true;
    if (ans === "n" || ans === "no") return false;
    return defaultYes;
  } finally {
    rl.close();
  }
}

/** Best-effort open a URL in the user's default browser. Returns true
 * if we successfully spawned an opener, false otherwise (the caller
 * should then fall back to printing the URL). */
function tryOpenBrowser(url: string): boolean {
  const p = platform();
  const cmd = p === "darwin" ? "open" : p === "win32" ? "cmd" : "xdg-open";
  const args = p === "win32" ? ["/c", "start", "", url] : [url];
  try {
    const child = spawn(cmd, args, {
      stdio: "ignore",
      detached: true,
    });
    child.unref();
    return true;
  } catch {
    return false;
  }
}

/** Substituted by tsup's `define` (see tsup.config.ts) — a string
 * literal like "agent-0.0.33" in the bundle. Falls back to "agent-dev"
 * when run unbundled (tsx / tests), where the define isn't applied. */
const AGENT_VERSION =
  typeof __MODELSTAT_VERSION__ === "string" ? __MODELSTAT_VERSION__ : "agent-dev";

function osFamily(): "macos" | "linux" | "other" {
  const p = platform();
  if (p === "darwin") return "macos";
  if (p === "linux") return "linux";
  return "other";
}

function osArch(): "x86_64" | "arm64" | "other" {
  const a = cpuArch();
  if (a === "x64") return "x86_64";
  if (a === "arm64") return "arm64";
  return "other";
}

/**
 * The device UUID this machine SHOULD use, derived deterministically
 * from its stable hardware/OS machine key (see machine-key.ts). Same
 * machine → same UUID forever, so a machine that lost ~/.modelstat
 * re-derives the exact UUID it had and the server maps it back to the
 * existing device row instead of minting a duplicate.
 *
 * Escape hatch: set MODELSTAT_DEVICE_SALT to run a SECOND logical
 * device on one machine (CI matrices, multi-tenant boxes). It's a
 * deterministic salt, not randomness — so even that stays idempotent
 * across reinstalls. Absent it, one machine is exactly one device.
 */
function intendedDeviceUuid(): string {
  const salt = process.env.MODELSTAT_DEVICE_SALT?.trim();
  const key = salt ? `${machineKey()}:${salt}` : machineKey();
  return deviceUuidFromMachineKey(key);
}

/**
 * Self-register: generate a UUIDv7 client-side, POST it to the server, and
 * cache the returned device_secret + claim_code in the local state file.
 *
 * After this returns, the agent is "registered but unclaimed" — it can
 * heartbeat, report discovery, and poll its own state. The operator (or
 * a human admin) needs to visit `claim_url` and sign in to attach the
 * device to an org. Until then, ingest of session data is rejected.
 */
async function cmdSelfRegister(): Promise<void> {
  // Device UUID resolution, in order of preference:
  //   1. A UUID already in identity.json — keep it, never churn an
  //      existing enrollment (this is what stops a re-register from
  //      orphaning the device the user already claimed).
  //   2. Otherwise derive it deterministically from this machine's
  //      stable hardware key. A fresh install (or one whose
  //      ~/.modelstat was wiped) thus lands on the SAME UUID the
  //      machine had before — the server dedupes it back to the
  //      existing row instead of creating a duplicate device.
  const deviceUuid = state.deviceUuid ?? intendedDeviceUuid();
  const derived = !state.deviceUuid;
  const mid = machineKey();

  const fingerprint: Record<string, string | number | boolean> = {
    hostname: hostname(),
    os_family: osFamily(),
    os_version: release(),
    arch: osArch(),
    companion: "modelstat-agent-dev",
    companion_version: AGENT_VERSION,
    // Stable, install-method-independent machine key. The server
    // dedupes self-register on this so the same physical machine can
    // never become two device rows, even if the UUID somehow differs
    // (legacy random UUID → deterministic UUID transition).
    machine_id: mid,
  };

  if (derived) {
    process.stdout.write(
      `  \x1b[2mdevice id derived from machine key (${machineKeySource()}): ${deviceUuid.slice(0, 8)}…\x1b[0m\n`,
    );
  }
  process.stdout.write(`  \x1b[2m→ POST ${state.apiUrl}/v1/devices/self-register\x1b[0m\n`);

  const res = await selfRegister({
    device_uuid: deviceUuid,
    fingerprint,
  });

  // Seed the canonical identity file atomically. Single write
  // (not five separate setters) so the file is never half-populated
  // if the process dies mid-sequence.
  state.saveFreshIdentity({
    deviceUuid: res.device_uuid,
    deviceId: res.device_id,
    bearerToken: res.device_secret,
    claimCode: res.claim_code,
    claimUrl: res.claim_url,
  });

  process.stdout.write(`  \x1b[32m✓\x1b[0m registered  device_id=${res.device_id}\n`);
  process.stdout.write(
    `  \x1b[32m✓\x1b[0m secret      ${res.secret_prefix}…  (hashed on server, never re-sent)\n`,
  );
  process.stdout.write(`  \x1b[32m✓\x1b[0m claim code  ${res.claim_code}\n`);
}

/** Poll the server until the device shows up as claimed (or the operator
 * Ctrl-Cs). Quietly returns once the device has a user_id. */
async function cmdAwaitClaim(): Promise<void> {
  const secret = state.bearer;
  if (!secret) {
    console.error("not registered — run `modelstat self-register` first");
    process.exit(1);
  }
  const url = state.claimUrl ?? "(visit your dashboard)";
  console.log(`waiting for human to claim this device:\n    ${url}\n`);
  while (true) {
    let me;
    try {
      me = await fetchDeviceMe(secret);
    } catch (e) {
      console.error(`poll failed: ${(e as Error).message}`);
      await new Promise((r) => setTimeout(r, 5000));
      continue;
    }
    if (me.status === "claimed") {
      console.log(`✓ claimed by user_id=${me.user_id}`);
      return;
    }
    process.stdout.write(".");
    await new Promise((r) => setTimeout(r, 2000));
  }
}

interface ConnectOpts {
  json: boolean;
  noBrowser: boolean;
  /** Force a fresh self-register even if an identity file already
   * exists. Existing identity is backed up to identity.json.bak-<ts>.
   * Non-interactive shortcut equivalent to answering "n" to the reuse
   * prompt. */
  fresh: boolean;
  /** Skip all interactive prompts. Default when stdin is not a TTY
   * (e.g. tray-launched daemon). With `--yes` the behavior is:
   * reuse identity if present, else self-register. */
  yes: boolean;
}

/** Emit one NDJSON event to stdout. Only active when --json is set.
 * Schema v1: every event has `v`, `ts`, `event`, plus event-specific
 * fields. Renaming or removing fields is a breaking change; adding is
 * not. See integrations/harness-skills/modelstat-connect/README.md. */
function emitEvent(opts: ConnectOpts, event: string, fields: Record<string, unknown> = {}): void {
  if (!opts.json) return;
  process.stdout.write(`${JSON.stringify({ v: 1, ts: Date.now(), event, ...fields })}\n`);
}

/**
 * Renders live progress for the on-device macOS tray compile.
 *
 * `bundledTrayAppPath` calls `onLine` once per line of SwiftPM output —
 * but ONLY when it actually has to build (a prebuilt .app short-circuits
 * with zero lines). So the UI lazily "begins" on the first line: it prints
 * a one-time heads-up, starts a 1s elapsed ticker (TTY only), and surfaces
 * each SwiftPM phase line ("[2/3] Compiling…", "Linking", "Build complete!")
 * as dim detail. `finish()` stops the ticker and returns the elapsed ms
 * (or null if no build ran). In `--json` mode it emits structured
 * tray_build_started / _progress / _done events instead of ANSI.
 */
function createTrayBuildUi(opts: ConnectOpts): {
  onLine: (line: string) => void;
  finish: () => number | null;
} {
  const isTty = !opts.json && process.stdout.isTTY === true;
  let startedAt: number | null = null;
  let ticker: ReturnType<typeof setInterval> | null = null;

  const paintTicker = (): void => {
    if (startedAt === null) return;
    const s = Math.round((Date.now() - startedAt) / 1000);
    process.stdout.write(`\r  \x1b[2m⏳ compiling menu-bar tray from source… ${s}s\x1b[0m\x1b[K`);
  };

  const begin = (): void => {
    if (startedAt !== null) return;
    startedAt = Date.now();
    emitEvent(opts, "tray_build_started", {});
    if (!opts.json) {
      process.stdout.write(
        "  \x1b[2mno prebuilt tray found — compiling a small Swift app locally " +
          "(first run only, ~1 min)\x1b[0m\n",
      );
    }
    if (isTty) {
      paintTicker();
      ticker = setInterval(paintTicker, 1000);
      ticker.unref?.();
    }
  };

  return {
    onLine: (line: string): void => {
      begin();
      emitEvent(opts, "tray_build_progress", { line });
      if (isTty) {
        // Commit this phase line above the ticker, which repaints on its
        // next tick (\r returns to col 0, \x1b[K clears the ticker text).
        process.stdout.write(`\r\x1b[K  \x1b[2m${line}\x1b[0m\n`);
      } else if (!opts.json) {
        process.stdout.write(`  ${line}\n`);
      }
    },
    finish: (): number | null => {
      if (ticker) {
        clearInterval(ticker);
        ticker = null;
      }
      if (isTty && startedAt !== null) process.stdout.write("\r\x1b[K");
      const elapsed = startedAt === null ? null : Date.now() - startedAt;
      if (elapsed !== null) emitEvent(opts, "tray_build_done", { elapsed_ms: elapsed });
      return elapsed;
    },
  };
}

/**
 * `modelstat connect` — the primary onboarding command.
 *
 * We talk loudly through every step. Silence on `npx modelstat@latest
 * connect` is the worst UX regression we've hit — the user is staring
 * at a blank terminal wondering if they just hung their shell.
 *
 * Steps (each prints a live progress line):
 *   1. Generate or reuse a UUIDv7 device identity.
 *   2. POST /v1/devices/self-register → get device_secret + claim_code.
 *   3. Install the macOS tray + background service (launchd/systemd).
 *   4. Print the /device/:claim_code URL and try to open it in a browser.
 *
 * The claim_code IS the capability — the user visits
 * /device/:claim_code, signs in, and clicks "Claim this device".
 */
async function cmdConnect(opts: ConnectOpts): Promise<void> {
  const step = (msg: string) => {
    if (opts.json) return;
    process.stdout.write(`\x1b[1;36m▸\x1b[0m ${msg}\n`);
  };
  const ok = (msg: string) => {
    if (opts.json) return;
    process.stdout.write(`  \x1b[32m✓\x1b[0m ${msg}\n`);
  };
  const warn = (msg: string) => {
    if (opts.json) return;
    process.stdout.write(`  \x1b[33m⚠\x1b[0m ${msg}\n`);
  };

  // ── 1. Self-register (or re-use existing identity) ─────────────
  //
  // Identity lives at ~/.modelstat/identity.json. Reinstalling the
  // CLI, relaunching the tray, or wiping the `conf` file does NOT
  // wipe this file — so `modelstat connect` twice on the same
  // machine resumes the existing enrollment instead of minting a
  // new device row.
  //
  // Re-registering is now CONVERGENT, not destructive: clearing the
  // in-memory bearer drops deviceUuid back to null, so cmdSelfRegister
  // re-derives the SAME deterministic UUID from the machine key and
  // sends machine_id — the server dedupes both back onto the existing
  // device row. So even `--fresh` and a credentials-rejected recovery
  // land on the same device instead of orphaning it into a duplicate.
  // (We still back the old file up first, purely for forensics.)
  const wipeAndSelfRegister = async (reason: string): Promise<void> => {
    warn(`${reason} — re-registering this device`);
    const bak = backupIdentity();
    if (bak) warn(`old identity moved to ${bak}`);
    state.setBearer(null);
    await cmdSelfRegister();
  };

  if (opts.fresh && hasIdentityFile()) {
    // Explicit re-register. Recovers/rotates onto the SAME device
    // (deterministic UUID); use MODELSTAT_DEVICE_SALT to intentionally
    // split a machine into a second logical device.
    step("`--fresh` passed — re-registering this device");
    await wipeAndSelfRegister("forced fresh start");
  } else if (!state.deviceUuid || !state.bearer || !state.deviceId) {
    step("Registering this device with modelstat.ai");
    await cmdSelfRegister();
  } else {
    // Have a local identity — confirm it's still valid server-side.
    step("Re-using existing device identity");
    ok(`device ${state.deviceId}`);
    ok(`identity file ${identityPath()}`);
    try {
      const me = await fetchDeviceMe(state.bearer);
      if (me.claim_code && me.claim_code !== state.claimCode) {
        state.setClaimCode(me.claim_code);
        ok(`claim code refreshed from server`);
      }
      if (me.claim_url && me.claim_url !== state.claimUrl) {
        state.setClaimUrl(me.claim_url);
      }
    } catch (e) {
      if (e instanceof DeviceMeUnauthorized) {
        // Server doesn't recognise the bearer any more. Ask before
        // overwriting — we don't want a stray 401 during network
        // weirdness to silently wipe a user's identity.
        const interactive = !opts.yes && process.stdin.isTTY === true;
        if (interactive) {
          const prompt =
            "cached credentials no longer accepted by the server. " +
            "Re-register this device? [Y/n] ";
          const answer = await confirmPrompt(prompt, true);
          if (!answer) {
            warn("keeping existing identity; connect aborted");
            return;
          }
        }
        await wipeAndSelfRegister("cached credentials no longer valid");
      } else {
        warn(`couldn't refresh device state: ${(e as Error).message}`);
      }
    }
  }

  const apiBase = state.apiUrl.replace(/\/$/, "");
  const claimCode = state.claimCode ?? "(unknown)";
  const claimUrl = state.claimUrl ?? `${apiBase}/device/${claimCode}`;
  const agentUrl = `${apiBase}/device/${claimCode}/agent`;
  emitEvent(opts, "registered", {
    device_uuid: state.deviceUuid,
    device_id: state.deviceId,
    claim_code: claimCode,
    claim_url: claimUrl,
    agent_url: agentUrl,
  });

  // ── 2. macOS menu-bar tray (best-effort) ──────────────────────
  // When no prebuilt .app ships, we compile the tray from source on the
  // user's machine — a cold `swift build` is ~1 min. The build UI streams
  // live progress (heads-up + elapsed ticker + SwiftPM phase lines) so
  // that minute doesn't read as a frozen terminal.
  if (platform() === "darwin") {
    step("Installing menu-bar tray (macOS)");
    const buildUi = createTrayBuildUi(opts);
    try {
      const src = await bundledTrayAppPath({ onLine: buildUi.onLine });
      const buildMs = buildUi.finish();
      if (src) {
        if (buildMs !== null) ok(`tray compiled from source in ${Math.round(buildMs / 1000)}s`);
        const out = installTrayApp(src);
        if (out) {
          emitEvent(opts, "tray_installed", {
            path: out.installedAt,
            ...(buildMs !== null ? { build_ms: buildMs } : {}),
          });
          ok(`tray at ${out.installedAt}`);
        }
      } else {
        emitEvent(opts, "tray_not_bundled", {});
        warn("no bundled tray — skipping (install Xcode CLI tools and re-run to get the icon)");
      }
    } catch (e) {
      buildUi.finish();
      emitEvent(opts, "tray_install_failed", { error: (e as Error).message });
      warn(`tray install skipped: ${(e as Error).message}`);
    }
  }

  // ── 3. Local summariser model ──────────────────────────────────
  // The bundled summariser produces every segment's abstract on
  // your machine. The model is ~2.7 GB and downloads to
  // ~/.modelstat/models/ on first use. Pull it BEFORE installing
  // the background service so the daemon's first preflight succeeds
  // and real abstracts begin streaming immediately on boot. If the
  // npm postinstall already pulled it, this returns instantly.
  step("Preparing local summariser (downloads on first run)");
  let modelReady = false;
  try {
    const { ensureLlamaModel, defaultLlamaConfig } = await import("@modelstat/companion-core/node");
    await ensureLlamaModel(defaultLlamaConfig());
    modelReady = true;
    emitEvent(opts, "summariser_model_ready", {});
    ok("summariser model on disk");
  } catch (e) {
    emitEvent(opts, "summariser_model_failed", {
      error: (e as Error).message,
    });
    warn(`couldn't prepare summariser model: ${(e as Error).message}`);
    warn("the background service will retry the download on its first scan");
  }

  // ── 4. Background service (install OR refresh-and-restart) ────
  // Idempotent: a fresh install creates the launchd plist / systemd
  // unit; a re-run on an already-installed machine refreshes the
  // bundle copy at ~/.modelstat/bin/modelstat.mjs and bounces the
  // service so the new code loads. The user always sees "service
  // installed and running" by the end.
  step("Installing/refreshing background service so the agent survives reboots");
  let serviceOk = false;
  try {
    const svc = installService();
    serviceOk = true;
    emitEvent(opts, "service_installed", {
      path: svc.path,
      logs: svc.logs,
      summariser_ready: modelReady,
    });
    ok(`${platform() === "darwin" ? "launchd" : "systemd --user"}: ${svc.path}`);
  } catch (e) {
    emitEvent(opts, "service_install_failed", { error: (e as Error).message });
    warn(`couldn't install service: ${(e as Error).message}`);
    warn("the agent will not run in the background — re-run after fixing the issue");
  }

  // ── 5. Detect local AI installs + signed-in accounts ──────────
  // Idempotent: a second `npx modelstat@latest` after the user signs
  // into a new account (e.g. fresh codex login, new Claude Keychain
  // entry) re-enumerates everything and reports it now, instead of
  // waiting up to a minute for the daemon's discovery interval to
  // fire on the next tick. The daemon also picks this up on restart,
  // but doing it inline here means the success banner can show the
  // real count and the user gets immediate confirmation.
  step("Detecting installed AI tools and signed-in accounts");
  let discovered: { installations: number; identities: number } | null = null;
  if (state.deviceId) {
    try {
      const d = await discover();
      await reportDiscovery({
        device_id: state.deviceId,
        installations: d.installations,
        identities: d.identities,
        scanned_at: new Date().toISOString(),
      });
      discovered = {
        installations: d.installations.length,
        identities: d.identities.length,
      };
      emitEvent(opts, "discovered", discovered);
      ok(`${discovered.installations} installs · ${discovered.identities} accounts`);
    } catch (e) {
      emitEvent(opts, "discovery_failed", { error: (e as Error).message });
      warn(`couldn't detect accounts: ${(e as Error).message}`);
    }
  }

  if (!opts.json) {
    const tray = trayStatus();
    const line = "━".repeat(60);
    console.log();
    console.log(line);
    console.log(`  ✓ Device registered — streaming your AI usage to modelstat.`);
    console.log();
    console.log(
      `    service : \x1b[${serviceOk ? "32" : "33"}m${serviceOk ? "installed" : "foreground"}\x1b[0m`,
    );
    if (discovered) {
      console.log(
        `    detected: \x1b[32m${discovered.installations} installs · ${discovered.identities} accounts\x1b[0m`,
      );
    }
    if (platform() === "darwin") {
      console.log(
        `    tray    : \x1b[${tray.installed ? "32" : "2"}m${tray.installed ? "menu-bar icon ready" : "not installed"}\x1b[0m`,
      );
    }
    console.log();
    console.log(`  Open your dashboard (no sign-up needed):`);
    console.log(`    \x1b[1;36m${claimUrl}\x1b[0m`);
    console.log();
    console.log(`  Live numbers from this terminal:`);
    console.log(`    \x1b[2mmodelstat stats\x1b[0m   # sessions · tokens · cost`);
    console.log(`    \x1b[2mmodelstat jobs\x1b[0m    # pipeline queue + recent activity`);
    console.log();
    console.log(`  Agent-friendly (for LLMs / MCPs):`);
    console.log(`    \x1b[2m${agentUrl}\x1b[0m`);
    console.log();
    console.log(`  Claim this device so it keeps analyzing past 100M tokens/mo:`);
    console.log(`    \x1b[2m${claimUrl}/claim\x1b[0m`);
    console.log(line);
    console.log();
  }

  // Try to open the claim URL in the user's browser so the tab is
  // already pointing at their data when they alt-tab away from the CLI.
  if (!opts.noBrowser) {
    const opened = tryOpenBrowser(claimUrl);
    emitEvent(opts, "browser_open_attempted", { opened });
  }

  emitEvent(opts, "done", { claim_url: claimUrl, agent_url: agentUrl });

  if (serviceOk) {
    return; // exit cleanly
  }

  if (opts.json) {
    // In JSON mode, don't drop into a blocking daemon — callers (skills)
    // expect a deterministic exit.
    return;
  }

  // No service support → run in the foreground so the user at least
  // gets data flowing.
  console.log("  Service install not supported on this platform — running in foreground.");
  console.log("  Press Ctrl-C to stop.");
  console.log();
  const { runDaemon } = await import("./daemon.js");
  await runDaemon();
}

async function cmdDiscover(): Promise<void> {
  const deviceId = state.deviceId;
  if (!deviceId) throw new Error("run `register` first");
  const out = await discover();
  console.log(`→ ${out.installations.length} installations, ${out.identities.length} identities`);
  await reportDiscovery({
    device_id: deviceId,
    installations: out.installations,
    identities: out.identities,
    scanned_at: new Date().toISOString(),
  });
  console.log("✓ reported to backend");
}

async function cmdScan(): Promise<void> {
  // Verify the summariser actually works on this machine BEFORE we
  // start producing segments. Otherwise the user discovers it's broken
  // by way of useless abstracts in the dashboard a day later. See
  // packages/companion-core/src/pipeline/index.ts: a thrown summariser
  // failure now propagates instead of silently writing the metadata
  // template.
  const { preflightSummariser } = await import("./pipeline.js");
  const sample = await preflightSummariser();
  console.log(`[modelstat] summariser preflight ok: "${sample}"`);
  const { reconcileProcessingVersion } = await import("./processing-version.js");
  const pv = reconcileProcessingVersion(state);
  if (pv.changed) {
    console.log(
      `[modelstat] processing pipeline v${pv.from} → v${pv.to} — wiped file cursors so every session is re-processed`,
    );
  }
  const r = await scanAll();
  console.log();
  console.log(
    `Done: ${r.filesScanned} files scanned, ${r.filesUnchanged} unchanged, ${r.batchesUploaded} batches, ${r.segmentsUploaded} segments, ${r.eventsUploaded} events uploaded`,
  );
}

/**
 * Manual force re-scan: wipe every file cursor + bump the local
 * processing-version stamp so the next scan re-reads all JSONLs
 * from byte 0 and re-summarises every session. Same effect as
 * shipping a new processing-version, just user-triggered (e.g. when
 * upstream changed something the agent doesn't know about, or when
 * the user wants to backfill old sessions for a freshly-claimed
 * device).
 */
async function cmdRescan(): Promise<void> {
  const { PROCESSING_VERSION } = await import("./processing-version.js");
  state.wipeCursors();
  state.setProcessingVersion(PROCESSING_VERSION);
  console.log(
    `[modelstat] cursors wiped — next \`modelstat scan\` (or daemon scan cycle) will re-read every JSONL from the start and re-summarise every session at processing version v${PROCESSING_VERSION}.`,
  );
  console.log("  If the daemon is running, kick it with: modelstat stop && modelstat start");
}

async function cmdWatch(): Promise<void> {
  const { watchForever } = await import("./watch.js");
  await watchForever();
}

async function cmdStart(rest: string[]): Promise<void> {
  if (!state.bearer || !state.deviceId) {
    console.error("not paired yet. Run `modelstat` first.");
    process.exit(1);
  }
  const force = rest.includes("--force") || rest.includes("-f");
  const { runDaemon } = await import("./daemon.js");
  await runDaemon({ force });
}

async function cmdStop(): Promise<void> {
  try {
    uninstallService();
    console.log("✓ service stopped and uninstalled");
    console.log(`  Your device pairing is still in ${state.storePath}`);
    console.log("  Run `modelstat` again to re-enable.");
  } catch (err) {
    console.error(`✗ ${(err as Error).message}`);
    process.exit(1);
  }
}

async function cmdStatus(): Promise<void> {
  const s = serviceStatus();
  const paired = !!state.bearer && !!state.deviceId;
  console.log(`paired:  ${paired ? "yes" : "no"}`);
  if (paired) {
    console.log(`  user:    ${state.userEmail ?? "(unknown)"}`);
    console.log(`  device:  ${state.deviceId}`);
    console.log(`  uuid:    ${state.deviceUuid ?? "(not self-registered)"}`);
  }
  console.log(`service: ${s.running ? "running" : "stopped"}  (${s.hint})`);
  console.log(`logs:    ${logsDir()}`);
  console.log(`state:   ${state.storePath}`);
  console.log(`api:     ${state.apiUrl}`);
}

function fmtInt(n: number | string): string {
  return Number(n).toLocaleString("en-US");
}

function fmtCost(usd: number): string {
  return `$${usd.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

function fmtTokens(v: string | number): string {
  const n = typeof v === "string" ? Number(v) : v;
  if (!Number.isFinite(n)) return "—";
  if (n >= 1e9) return `${(n / 1e9).toFixed(n >= 1e10 ? 0 : 1)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(0)}K`;
  return String(n);
}

/** Live ingestion stats for the agent's device. Pulled from the
 * claim-code capability endpoint so we work for unclaimed devices
 * (the common "I just ran connect" case). Claimed devices get a
 * "see the dashboard" pointer — the CLI doesn't carry the cookie
 * needed to read the authenticated dashboard endpoints. */
/** Best-effort read of the daemon's heartbeat mirror. Returns the
 * parsed object or null if the file isn't there / can't be parsed. */
async function readLocalStatus(): Promise<Record<string, unknown> | null> {
  try {
    const { homedir } = await import("node:os");
    const { join } = await import("node:path");
    const { readFile } = await import("node:fs/promises");
    const p = join(homedir(), ".modelstat", "last-status.json");
    const txt = await readFile(p, "utf8");
    return JSON.parse(txt) as Record<string, unknown>;
  } catch {
    return null;
  }
}

async function cmdStats(args: readonly string[]): Promise<void> {
  const asJson = args.includes("--json");
  const claim = state.claimCode;
  // Read the daemon's local heartbeat snapshot so we can answer for
  // claimed devices (where the public device-view endpoint 404s for
  // non-owners) and so unclaimed devices get the live progress
  // numbers the tray polls. File is written by sendHeartbeat() in
  // daemon.ts every ~10s; absence just means the daemon hasn't
  // started yet or this build is too old to write it.
  const local = await readLocalStatus();
  if (!claim) {
    if (asJson) {
      process.stdout.write(
        `${JSON.stringify({
          paired: false,
          reason: "no_claim_code",
          local,
        })}\n`,
      );
    } else {
      console.log("no claim code on record — run `npx modelstat@latest` first");
    }
    return;
  }
  const view = await fetchDeviceViewByClaim(claim);
  if (!view) {
    // 404 means the device was claimed and its data moved to the
    // user's personal org. Return the local snapshot so the tray
    // can still show segment / identity / installation counts.
    const dashboard = `${state.apiUrl.replace(/\/$/, "")}/dashboard`;
    if (asJson) {
      process.stdout.write(
        `${JSON.stringify({
          paired: true,
          claimed: true,
          dashboard,
          local,
        })}\n`,
      );
    } else {
      console.log("device is claimed — live stats available at:");
      console.log(`  ${dashboard}`);
      if (local) {
        console.log(
          `local agent: ${local.status ?? "?"}${local.message ? ` · ${local.message}` : ""}`,
        );
        const stats = (local.stats as Record<string, number | string> | undefined) ?? {};
        for (const [k, v] of Object.entries(stats)) console.log(`  ${k}: ${v}`);
      }
    }
    return;
  }
  if (asJson) {
    process.stdout.write(`${JSON.stringify({ ...view, local })}\n`);
    return;
  }
  console.log(`device:   ${view.device.id}`);
  console.log(`host:     ${view.device.hostname ?? "(unknown)"} (${view.device.os_family ?? "?"})`);
  console.log(`companion: ${view.device.companion_version ?? "(unknown)"}`);
  console.log(
    `status:   ${view.device.companion_status ?? "(unknown)"}${view.device.last_seen_at ? ` · last seen ${view.device.last_seen_at}` : ""}`,
  );
  console.log(
    `claim:    ${view.status}${view.status === "unclaimed" ? ` (at ${view.claim_url})` : ""}`,
  );
  console.log(`sessions: ${fmtInt(view.analyzed.count)}`);
  console.log(`tokens:   ${fmtTokens(view.analyzed.totalTokens)}`);
  console.log(`cost:     ${fmtCost(view.analyzed.totalCostUsd)}`);
  console.log(`quota:    ${fmtTokens(view.free_quota_tokens)} free-tier ceiling`);
}

/** Background-pipeline view: how many jobs are queued / in-flight +
 * the last handful of ledger events (ingest / reanalyze / credit). */
async function cmdJobs(args: readonly string[]): Promise<void> {
  const asJson = args.includes("--json");
  const claim = state.claimCode;
  if (!claim) {
    if (asJson) {
      process.stdout.write(`${JSON.stringify({ paired: false, reason: "no_claim_code" })}\n`);
    } else {
      console.log("no claim code on record — run `modelstat` first");
    }
    return;
  }
  const [jobs, ledger] = await Promise.all([
    fetchDeviceViewJobsByClaim(claim),
    fetchDeviceViewLedgerByClaim(claim),
  ]);
  if (!jobs && !ledger) {
    const dashboard = `${state.apiUrl.replace(/\/$/, "")}/dashboard/jobs`;
    if (asJson) {
      process.stdout.write(`${JSON.stringify({ paired: true, claimed: true, dashboard })}\n`);
    } else {
      console.log("device is claimed — job queue at:");
      console.log(`  ${dashboard}`);
    }
    return;
  }
  if (asJson) {
    process.stdout.write(
      `${JSON.stringify({ jobs: jobs ?? null, ledger: ledger?.recent ?? [] })}\n`,
    );
    return;
  }
  console.log(`queue:   ${jobs?.pending ?? 0} pending · ${jobs?.running ?? 0} running`);
  const recent = ledger?.recent ?? [];
  if (recent.length === 0) {
    console.log("ledger:  (no activity yet)");
    return;
  }
  console.log(`ledger:  (last ${recent.length})`);
  for (const r of recent.slice(0, 15)) {
    const ts = r.occurred_at.slice(0, 19).replace("T", " ");
    console.log(
      `  ${ts}  ${r.kind.padEnd(16)}  ${fmtTokens(r.tokens).padStart(7)} tok  ${r.session_id ?? ""}`,
    );
  }
}

/** Print the resolved state file + log paths as JSON. Read by the
 * @modelstat/mcp server (and any other tool) to locate the shared
 * state file without re-implementing Conf's path algorithm — so
 * brew-installed and npm-installed CLIs always point at the same
 * state, and MCP just shells out to `modelstat paths --json`. */
function cmdPaths(args: readonly string[]): void {
  // intended_uuid is what THIS machine would self-register as today
  // (derived from the hardware key). It should equal device_uuid for a
  // healthy enrollment; a mismatch means the stored identity predates
  // the deterministic scheme (still fine — it's reused as-is, and
  // machine_id dedupe covers it server-side).
  const intended = intendedDeviceUuid();
  const data = {
    state: state.storePath,
    identity: identityPath(),
    logs: logsDir(),
    api: state.apiUrl,
    paired: !!state.bearer && !!state.deviceId,
    device_id: state.deviceId ?? "(none)",
    device_uuid: state.deviceUuid ?? "(none)",
    intended_uuid: intended,
    machine_key_source: machineKeySource(),
  };
  if (args.includes("--json")) {
    process.stdout.write(`${JSON.stringify(data)}\n`);
    return;
  }
  for (const [k, v] of Object.entries(data)) {
    console.log(`${k.padEnd(8)} ${String(v)}`);
  }
}

/** Print the device token (bearer) for hosted-MCP / API setups. Bare token
 * on stdout so `$(npx -y modelstat@latest token)` substitutes cleanly; the
 * handle-with-care note goes to stderr. */
function cmdToken(args: readonly string[]): void {
  if (!state.bearer) {
    console.error("not paired — run `npx modelstat@latest` first");
    process.exit(1);
  }
  if (args.includes("--json")) {
    process.stdout.write(`${JSON.stringify({ token: state.bearer, api: state.apiUrl })}\n`);
    return;
  }
  process.stdout.write(`${state.bearer}\n`);
  if (process.stderr.isTTY) {
    console.error("(device token — treat it like a password; rotate via the dashboard if leaked)");
  }
}

function parseConnectOpts(argv: readonly string[]): ConnectOpts {
  return {
    json: argv.includes("--json"),
    noBrowser: argv.includes("--no-browser"),
    fresh: argv.includes("--fresh"),
    yes: argv.includes("--yes") || argv.includes("-y"),
  };
}

async function main(): Promise<void> {
  const cmd = process.argv[2];
  const rest = process.argv.slice(3);
  switch (cmd) {
    case undefined:
    case "connect":
      // Default action — install or upgrade. Registers the device if
      // it isn't already, installs the launchd / systemd service,
      // ensures it's running, and exits cleanly. Idempotent: a re-run
      // refreshes the bundle copy and bounces the service so the new
      // code loads. Users do NOT see the foreground daemon — that's
      // an internal entry point used by the service supervisor only.
      // `connect` stays as a hidden alias so older docs and shell
      // history keep working.
      return cmdConnect(parseConnectOpts(rest));
    case "remove":
    case "uninstall":
    case "stop":
      // Stop and remove the background service. Doesn't touch the
      // pairing identity at ~/.modelstat/identity.json, so a later
      // re-run picks the same device row up rather than minting a
      // new one. Pass --fresh on the next run to mint anew.
      return cmdStop();
    case "reinstall":
      // Same as the default action, but explicit — mostly here for
      // discoverability. `npx modelstat@latest` already does the
      // right thing on a re-run.
      return cmdConnect(parseConnectOpts(rest));

    // ── Internal / service entry point ─────────────────────────
    case "_daemon":
    case "start":
      // Foreground daemon. The launchd plist + systemd unit invoke
      // this. Not advertised in --help; users get to background work
      // via the default install flow above.
      return cmdStart(rest);
    case "_setup-runtime": {
      // Internal: copy the bundle to ~/.modelstat/bin AND stage the
      // native summariser runtime beside it, so the install is
      // self-contained (npm-only, no Ollama). Run straight from the
      // freshly-unpacked npm tree by the install pipeline. No-op-safe.
      const dest = setupRuntime();
      console.log(`✓ runtime staged at ${dest}`);
      return;
    }
    case "_daemon-health": {
      // Internal: one-line JSON verdict for supervisors (the macOS
      // tray) deciding whether to adopt the live daemon, spawn a
      // fresh one, or force-replace a wedged one — see supervise.ts.
      // Never throws: a broken probe must not strand the supervisor,
      // so any failure degrades to "spawn".
      try {
        console.log(JSON.stringify(daemonHealth({ myCompanionVersion: AGENT_VERSION })));
      } catch (e) {
        console.log(JSON.stringify({ decision: "spawn", error: (e as Error).message }));
      }
      return;
    }

    // ── Diagnostics / dev one-shots ────────────────────────────
    case "status":
      return cmdStatus();
    case "stats":
      return cmdStats(rest);
    case "jobs":
      return cmdJobs(rest);
    case "paths":
      cmdPaths(rest);
      return;
    case "token":
      cmdToken(rest);
      return;
    case "discover":
      return cmdDiscover();
    case "scan":
      return cmdScan();
    case "rescan":
      return cmdRescan();
    case "watch":
      return cmdWatch();
    case "self-register":
      return cmdSelfRegister();
    case "await-claim":
      return cmdAwaitClaim();
    default:
      console.log("usage:");
      console.log(
        "  npx modelstat@latest                — install or upgrade. Registers the device, installs the background service, exits.",
      );
      console.log(
        "                                        flags: --json (NDJSON events on stdout), --no-browser, --fresh, -y",
      );
      console.log(
        "  npx modelstat@latest remove         — stop and uninstall the background service. Keeps your identity.",
      );
      console.log(
        "  npx modelstat@latest reinstall      — alias for the default. Useful when you want to be explicit.",
      );
      console.log();
      console.log("Diagnostics:");
      console.log("  npx modelstat@latest status         — show pairing + service state");
      console.log(
        "  npx modelstat@latest stats          — live device summary: sessions · tokens · cost (--json)",
      );
      console.log(
        "  npx modelstat@latest jobs           — pipeline queue + recent processing ledger (--json)",
      );
      console.log(
        "  npx modelstat@latest paths          — print state file + log dir + api URL (--json)",
      );
      console.log(
        "  npx modelstat@latest token          — print the device token for hosted MCP / API access (--json)",
      );
      console.log();
      console.log("Dev / one-shots:");
      console.log("  npx modelstat@latest scan           — one-shot parse + upload of local JSONL");
      console.log(
        "  npx modelstat@latest rescan         — wipe file cursors so the next scan re-reads & re-summarises everything",
      );
      console.log(
        "  npx modelstat@latest watch          — continuous (chokidar) with periodic backstop",
      );
      console.log("  npx modelstat@latest discover       — one-shot report of installs/identities");
      console.log();
      console.log(
        "Internal (called by launchd/systemd, not by humans): _daemon, start, self-register, await-claim",
      );
      process.exit(1);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
