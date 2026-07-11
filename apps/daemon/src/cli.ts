/**
 * modelstat CLI: `connect`, `discover`, `sync`, `watch`, `status`, `jobs`.
 *
 * The bundled build (tsup → dist/cli.cjs) adds a `#!/usr/bin/env node`
 * shebang at pack time — keep this source shebang-free so we don't emit
 * two of them.
 */
import { spawn } from "node:child_process";
import { platform } from "node:os";
import { createInterface } from "node:readline";
import { DeviceMeUnauthorized, fetchDeviceMe, recoverIdentity, selfRegister } from "./api.js";
import { state } from "./config.js";
import { backupIdentity, hasIdentityFile, identityPath } from "./identity.js";
import { buildFingerprint, intendedDeviceUuid, machineKeySource } from "./machine-key.js";
import { parseSummarizerMode, type SummarizerMode } from "./runtime-state.js";
import {
  bundledTrayAppPath,
  installService,
  installTrayApp,
  installTrayAutostart,
  logsDir,
  refreshTrayIfInstalled,
  removeTrayApp,
  serviceStatus,
  setupRuntime,
  trayStatus,
  uninstallService,
  uninstallTrayAutostart,
} from "./service.js";
import { daemonHealth } from "./supervise.js";
import {
  autoUpdateEnabled,
  autoUpdatePinnedByEnv,
  runUpgrade,
  setStoredAutoUpdate,
  storedAutoUpdate,
} from "./update.js";

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

/** Free-text prompt over stdin. Returns `def` if stdin isn't a TTY or the user
 * just hits Enter. */
async function textPrompt(question: string, def = ""): Promise<string> {
  if (process.stdin.isTTY !== true) return def;
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  try {
    const raw: string = await new Promise((resolve) => rl.question(question, resolve));
    const ans = raw.trim();
    return ans === "" ? def : ans;
  } finally {
    rl.close();
  }
}

/**
 * Per-mode copy for the install chooser + `modelstat mode`. Each mode states
 * its RESOURCE profile (what it costs THIS machine) and its PRIVACY profile
 * (what leaves, and to where). Redaction runs on-device in EVERY mode — only
 * the summarisation LOCATION differs — so the privacy lines describe the
 * already-cleaned payload. The `local` resource line is the explicit
 * RAM/battery warning a tester asked us to surface up front.
 */
const MODE_INFO: Record<SummarizerMode, { label: string; resource: string; privacy: string }> = {
  cloud: {
    label: "Cloud (default) — modelstat's servers summarise",
    resource: "no local model; negligible RAM, CPU and battery on this machine",
    privacy: "your cleaned, redacted turns are uploaded and summarised server-side",
  },
  local: {
    label: "Local — a bundled model summarises on THIS machine",
    resource:
      "⚠ downloads a ~2.7 GB model (Qwen3-4B) and uses ~4 GB RAM plus extra battery/CPU while summarising",
    privacy: "only a ≤240-char abstract is uploaded; the raw turns never leave this machine",
  },
  "self-hosted": {
    label: "Self-hosted — your org's own AI endpoint summarises",
    resource: "no local model here; summarisation runs on the endpoint you configure",
    privacy:
      "only the abstract reaches modelstat; the cleaned excerpts go to your own endpoint (URL + model)",
  },
};

/** Interactive menu (Cloud pre-selected) → a {@link SummarizerMode}. Only
 * called when stdin is a TTY; returns the default on empty input. Each option
 * lists its resource cost and privacy profile so the choice is informed. */
async function promptForMode(current: SummarizerMode): Promise<SummarizerMode> {
  const def = current;
  const opt = (n: string, m: SummarizerMode): string =>
    `  ${n}) ${MODE_INFO[m].label}\n` +
    `       resource: ${MODE_INFO[m].resource}\n` +
    `       privacy:  ${MODE_INFO[m].privacy}\n`;
  process.stdout.write(
    "\nHow should modelstat summarise your sessions?\n" +
      "Redaction (secrets + names/PII) ALWAYS runs on your machine first — only the\n" +
      "summarisation LOCATION below changes.\n\n" +
      opt("1", "cloud") +
      opt("2", "local") +
      opt("3", "self-hosted") +
      "\n",
  );
  const raw = await textPrompt(`Choose 1-3 or a name [${def}]: `, def);
  const byNumber: Record<string, SummarizerMode> = {
    "1": "cloud",
    "2": "local",
    "3": "self-hosted",
  };
  return byNumber[raw.trim()] ?? parseSummarizerMode(raw) ?? def;
}

/**
 * Resolve the summariser mode for an install / `modelstat mode` run and persist
 * it (plus the self-hosted endpoint when applicable). Precedence:
 *   1. explicit `requested` (a --mode flag / positional arg),
 *   2. an interactive menu (TTY only, Cloud pre-selected),
 *   3. the current persisted mode, unchanged.
 * For self-hosted it resolves the endpoint from --url/--model, else the
 * MODELSTAT_LLM_* env, else an interactive prompt, and validates the URL —
 * throwing (so the caller can surface it) when it can't get a valid endpoint.
 * Returns the chosen mode.
 */
async function resolveAndPersistMode(input: {
  requested?: string;
  url?: string;
  model?: string;
  interactive: boolean;
}): Promise<SummarizerMode> {
  let mode: SummarizerMode;
  if (input.requested !== undefined && input.requested !== "") {
    const parsed = parseSummarizerMode(input.requested);
    if (!parsed) {
      throw new Error(`unknown mode "${input.requested}" — expected cloud, local, or self-hosted`);
    }
    mode = parsed;
  } else if (input.interactive) {
    mode = await promptForMode(state.summarizerMode);
  } else {
    // Non-interactive with nothing requested: fresh installs default to cloud;
    // an existing choice is left as-is.
    mode = state.summarizerMode;
  }

  if (mode === "self-hosted") {
    // URL + model: flags → env → interactive prompt.
    let url = input.url ?? process.env.MODELSTAT_LLM_BASE_URL?.trim() ?? "";
    let model = input.model ?? process.env.MODELSTAT_LLM_MODEL?.trim() ?? "";
    if (!url && input.interactive) {
      url = await textPrompt(
        "  Self-hosted summariser URL (OpenAI-compatible, e.g. https://llm.acme.internal/v1): ",
      );
    }
    if (!model && input.interactive) {
      model = await textPrompt("  Model id (e.g. qwen2.5-7b-instruct): ");
    }
    if (!url) throw new Error("self-hosted mode needs a summariser URL (--url <URL>)");
    if (!model) throw new Error("self-hosted mode needs a model id (--model <ID>)");
    // Reuse the daemon-core validator so `modelstat mode` rejects a bad URL the
    // same way the pipeline would (http/https only).
    const { validateSummarizerUrl } = await import("@modelstat/daemon-core/node");
    validateSummarizerUrl(url, "summariser URL"); // throws on invalid
    state.setSelfHosted(url, model);
  } else {
    // Leaving self-hosted — clear the stored endpoint so a stale URL can't
    // linger and resurface if the user flips back without re-entering it.
    state.setSelfHosted("", "");
  }
  state.setSummarizerMode(mode);
  return mode;
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

/**
 * Wire the modelstat MCP into every local AI tool, by shelling out to the
 * single source of truth — `npx -y @modelstat/mcp wire`, the exact same
 * entry point install.sh / install.ps1 call. This is what lets a bare
 * `npx modelstat@latest` (no curl installer) configure Claude Code / Cursor
 * / Codex / VS Code / Zed / Windsurf, so the user can *ask* about the usage
 * the daemon is now streaming. Best-effort + idempotent: a missing `npx`, a
 * non-zero exit, or a hang (120s cap) never blocks onboarding.
 *
 * In `--json` mode wire's human report is swallowed (it would corrupt the
 * NDJSON stream) and the caller emits a structured event instead; otherwise
 * wire prints its own per-tool report under our step line, like install.sh.
 */
function wireMcpTools(json: boolean): Promise<{ ok: boolean; error?: string }> {
  return new Promise((resolve) => {
    let child: ReturnType<typeof spawn>;
    try {
      child = spawn("npx", ["-y", "@modelstat/mcp", "wire"], {
        stdio: json ? ["ignore", "ignore", "pipe"] : "inherit",
        timeout: 120_000,
      });
    } catch (e) {
      resolve({ ok: false, error: (e as Error).message });
      return;
    }
    let stderr = "";
    child.stderr?.on("data", (b: Buffer) => {
      stderr += String(b);
    });
    child.on("error", (e: Error) => resolve({ ok: false, error: e.message }));
    child.on("close", (code: number | null, signal: NodeJS.Signals | null) => {
      if (code === 0) resolve({ ok: true });
      else
        resolve({
          ok: false,
          error: stderr.trim() || (signal ? `killed (${signal})` : `exit ${code}`),
        });
    });
  });
}

/** Substituted by tsup's `define` (see tsup.config.ts) — a string
 * literal like "daemon-0.0.33" in the bundle. Falls back to "daemon-dev"
 * when run unbundled (tsx / tests), where the define isn't applied. */
const DAEMON_VERSION =
  typeof __MODELSTAT_VERSION__ === "string" ? __MODELSTAT_VERSION__ : "daemon-dev";

/** First value after `--flag` (supports `--flag v` and `--flag=v`), or
 * undefined. */
function flagValue(args: readonly string[], flag: string): string | undefined {
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    if (a === flag) return args[i + 1];
    if (a.startsWith(`${flag}=`)) return a.slice(flag.length + 1);
  }
  return undefined;
}

/** Every value of a repeatable `--flag` (e.g. `--session a --session b`). */
function flagValues(args: readonly string[], flag: string): string[] {
  const out: string[] = [];
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    if (a === flag) {
      const v = args[i + 1];
      if (v !== undefined && !v.startsWith("--")) out.push(v);
    } else if (a.startsWith(`${flag}=`)) {
      out.push(a.slice(flag.length + 1));
    }
  }
  return out;
}

/** Numeric value of `--flag`, or undefined when absent / non-numeric. */
function numericFlag(args: readonly string[], flag: string): number | undefined {
  const raw = flagValue(args, flag);
  if (raw === undefined) return undefined;
  const n = Number(raw);
  return Number.isFinite(n) ? n : undefined;
}

/** No human present: an explicit CI runner, or NO terminal on any std stream.
 *
 * We check stdin/stdout/stderr — a human is present if *any* is a TTY — and
 * deliberately do NOT key on stdin alone. The official `curl … | sh` installer
 * (and any piped-stdin run) leaves stdin a pipe while stdout/stderr stay the
 * user's terminal; a stdin-only check wrongly flagged that human as CI and
 * refused to register them. Truly headless CI has no TTY on any stream (and
 * usually sets `CI`), so it's still blocked. */
function isNonInteractive(): boolean {
  if (process.env.CI) return true;
  return !(
    process.stdin.isTTY === true ||
    process.stdout.isTTY === true ||
    process.stderr.isTTY === true
  );
}

/** True when an env var is set to a truthy value (`1`/`true`/`yes`). */
function envFlag(name: string): boolean {
  const v = process.env[name]?.trim().toLowerCase();
  return v === "1" || v === "true" || v === "yes";
}

/** Explicit "yes, really register against prod headlessly" opt-in. */
function prodRegisterOptIn(): boolean {
  return envFlag("MODELSTAT_ALLOW_PROD_REGISTER");
}

/**
 * Register this device through the ONE register door, POST /v1/tokens: derive a
 * machine-stable device_uuid + fingerprint, POST them, and cache the returned
 * device_secret (ds_live_…) + claim handle in the local identity file.
 *
 * After this returns, an UNCLAIMED device can heartbeat (which folds in
 * discovery) and poll its own state; the operator visits `claim_url`, signs in,
 * and attaches it to an org. A device the server recognises by machine_id comes
 * back already CLAIMED with a fresh secret (re_registered) — no duplicate row,
 * no dead claim link.
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

  // Guard: never silently create a NEW device on PRODUCTION from a
  // non-interactive / CI environment. Ephemeral CI + cloud sandbox runners
  // were self-registering against prod and piling up unclaimed device rows
  // (and pinging ops Slack). This only blocks a *fresh* enrollment with the
  // unoverridden prod default — an already-enrolled device re-registering
  // (a claimed user's installed service) is never touched, and interactive
  // `npx modelstat@latest` onboarding is unaffected.
  if (derived && state.isProdDefaultApi && isNonInteractive() && !prodRegisterOptIn()) {
    process.stderr.write(
      "modelstat: refusing to self-register a new device against production from a\n" +
        "non-interactive/CI environment (no claim is possible here anyway). Either:\n" +
        "  • point at your own backend:  DAEMON_API_URL=https://your-host   (CI/e2e)\n" +
        "  • explicitly opt in:          MODELSTAT_ALLOW_PROD_REGISTER=1\n" +
        "  • or run it interactively:    npx modelstat@latest\n",
    );
    process.exit(2);
  }

  // ONE fingerprint, shared with the heartbeat — see buildFingerprint().
  // `fingerprint.machine_id` is the server's dedupe anchor; it MUST be
  // byte-identical on register + heartbeat, so both read the same source.
  const fingerprint = buildFingerprint();

  if (derived) {
    process.stdout.write(
      `  \x1b[2mdevice id derived from machine key (${machineKeySource()}): ${deviceUuid.slice(0, 8)}…\x1b[0m\n`,
    );
  }
  process.stdout.write(`  \x1b[2m→ POST ${state.apiUrl}/v1/tokens\x1b[0m\n`);

  const res = await selfRegister({
    device_uuid: deviceUuid,
    fingerprint,
  });

  // Seed the canonical identity file atomically. Single write
  // (not five separate setters) so the file is never half-populated
  // if the process dies mid-sequence. The device_secret is stored verbatim
  // (ds_live_…) and sent as the Bearer — no client-side format assumptions.
  state.saveFreshIdentity({
    deviceUuid: res.device_uuid,
    deviceId: res.device_id,
    bearerToken: res.device_secret,
    claimCode: res.claim_code,
    claimUrl: res.claim_url,
  });

  process.stdout.write(
    `  \x1b[32m✓\x1b[0m ${res.re_registered ? "re-registered" : "registered"}  device_id=${res.device_id}\n`,
  );
  process.stdout.write(
    `  \x1b[32m✓\x1b[0m secret      ${res.secret_prefix}…  (hashed on server, never re-sent)\n`,
  );
  if (res.status === "claimed") {
    // Already attached to an account — there is NO live claim code/URL to print
    // (the server returns null), so don't dangle a dead link. Point at the
    // dashboard instead.
    process.stdout.write(
      `  \x1b[32m✓\x1b[0m already claimed${res.user_id ? ` by user_id=${res.user_id}` : ""} — open ${state.apiUrl.replace(/\/$/, "")}/dashboard\n`,
    );
  } else if (res.claim_code) {
    process.stdout.write(`  \x1b[32m✓\x1b[0m claim code  ${res.claim_code}\n`);
  }
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
      // Always read the CURRENT bearer — recoverIdentity() below may rotate it.
      me = await fetchDeviceMe(state.bearer ?? secret);
    } catch (e) {
      if (e instanceof DeviceMeUnauthorized) {
        // The server no longer accepts our bearer (revoked / row deleted). The
        // OLD code logged "poll failed" and re-polled the dead secret every 5s —
        // a busy-loop that could never recover. Recover the identity by
        // machine-stable re-register (recoverIdentity self-rate-limits with its
        // own exponential backoff so a truly-deleted row can't hot-loop), then
        // poll the fresh bearer.
        const recovered = await recoverIdentity();
        console.error(
          recovered
            ? "re-registered after the server rejected our credentials — resuming claim wait"
            : "couldn't re-register yet (server rejecting registration) — backing off",
        );
        // recoverIdentity already backed off internally on failure; add a small
        // floor so a same-tick success still paces the next poll.
        await new Promise((r) => setTimeout(r, recovered ? 2000 : 5000));
        continue;
      }
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
  /** Summariser mode from `--mode <cloud|local|self-hosted>`. When set, skips
   * the interactive chooser. */
  mode?: string;
  /** Self-hosted endpoint from `--url` / `--model` (used with `--mode self-hosted`). */
  selfHostedUrl?: string;
  selfHostedModel?: string;
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
 *   1. Derive (or reuse) a machine-stable device identity.
 *   2. POST /v1/tokens → get device_secret (ds_live_…) + claim_code.
 *   3. Install the macOS tray + background service (launchd/systemd).
 *   4. Print the dashboard / claim URL and try to open it in a browser.
 *
 * For an UNCLAIMED device the claim_code IS the capability — the user visits
 * /device/:claim_code, signs in, and clicks "Claim this device". An already
 * CLAIMED device has no live claim handle, so the banner points at the
 * dashboard instead of a dead claim link.
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

  // Determine whether this device is already attached to an account. A claimed
  // device has NO live claim code/URL (the server returns null), so the final
  // banner must point at the dashboard rather than dangle a dead claim link.
  // Best-effort: a failed probe (offline) falls back to the unclaimed banner.
  let claimed = false;
  if (state.bearer) {
    try {
      const me = await fetchDeviceMe(state.bearer);
      claimed = me.status === "claimed";
    } catch {
      /* offline / transient — assume unclaimed for banner purposes */
    }
  }

  const apiBase = state.apiUrl.replace(/\/$/, "");
  const dashboardUrl = `${apiBase}/dashboard`;
  const claimCode = state.claimCode ?? "(unknown)";
  const claimUrl = claimed ? dashboardUrl : (state.claimUrl ?? `${apiBase}/device/${claimCode}`);
  const agentUrl = `${apiBase}/device/${claimCode}/agent`;
  emitEvent(opts, "registered", {
    device_uuid: state.deviceUuid,
    device_id: state.deviceId,
    claimed,
    claim_code: claimed ? null : claimCode,
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
          // Install the tray's own launchd agent and start it now. This
          // shows the icon immediately AND, via RunAtLoad + KeepAlive,
          // brings it back on every login and restarts it if it crashes —
          // without the tray having to register itself as a Login Item.
          // Best-effort: the daemon is already running headless, so a
          // failure here costs only the menu-bar icon, not the pipeline.
          const trayAgent = installTrayAutostart();
          emitEvent(opts, "tray_autostart_installed", { ok: trayAgent !== null });
          if (trayAgent) ok("menu-bar icon launched (and set to start at login)");
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

  // ── 2.5 Summariser mode (Cloud pre-selected) ───────────────────
  // Where each session gets summarised. Redaction ALWAYS runs on this machine
  // first; only the summarisation LOCATION differs. Chosen interactively on a
  // TTY (Cloud pre-selected), or via --mode / MODELSTAT_SUMMARIZER_MODE for
  // scripted installs; a non-interactive install with no choice defaults to
  // cloud — so it never downloads the local model. Changeable later with
  // `modelstat mode`.
  step("Choosing where sessions get summarised (redaction stays on your machine)");
  try {
    await resolveAndPersistMode({
      requested: opts.mode,
      url: opts.selfHostedUrl,
      model: opts.selfHostedModel,
      interactive: !opts.yes && !opts.json && process.stdin.isTTY === true,
    });
  } catch (e) {
    warn(`mode selection failed: ${(e as Error).message}`);
    warn("falling back to cloud summarisation (change later with `modelstat mode`)");
    state.setSummarizerMode("cloud");
    state.setSelfHosted("", "");
  }
  // The EFFECTIVE mode the daemon will run (honours MODELSTAT_SUMMARIZER_MODE).
  const mode = state.summarizerMode;
  if (state.summarizerModeIsEnvOverridden) {
    warn(`MODELSTAT_SUMMARIZER_MODE is set — running "${mode}" regardless of the stored choice`);
  }
  ok(
    mode === "self-hosted"
      ? `summariser mode: self-hosted (${state.selfHosted.url})`
      : `summariser mode: ${mode}`,
  );
  emitEvent(opts, "summarizer_mode", {
    mode,
    ...(mode === "self-hosted" ? { url: state.selfHosted.url, model: state.selfHosted.model } : {}),
  });

  // ── 3. Local summariser model (local mode only) ────────────────
  // Only local mode runs the bundled model, so only local downloads it —
  // cloud/self-hosted skip the ~2.7 GB pull entirely. In local mode we pull it
  // BEFORE installing the service so the daemon's first preflight succeeds and
  // real abstracts stream immediately on boot; if the npm postinstall already
  // pulled it, this returns instantly.
  let modelReady = false;
  if (mode === "local") {
    step("Preparing local summariser (downloads on first run)");
    try {
      const { ensureLlamaModel, defaultLlamaConfig } = await import("@modelstat/daemon-core/node");
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
  } else {
    emitEvent(opts, "summariser_model_skipped", { mode });
    ok(
      mode === "cloud"
        ? "cloud mode — modelstat summarises server-side (no local model to download)"
        : "self-hosted mode — your endpoint summarises (no local model to download)",
    );
  }

  // ── 4. Background service (install OR refresh-and-restart) ────
  // Idempotent: a fresh install creates the launchd plist / systemd
  // unit; a re-run on an already-installed machine refreshes the
  // bundle copy at ~/.modelstat/bin/modelstat.mjs and bounces the
  // service so the new code loads. The user always sees "service
  // installed and running" by the end.
  step("Installing/refreshing background service so the daemon survives reboots");
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
    warn("the daemon will not run in the background — re-run after fixing the issue");
  }

  // ── 4.5 Claude Code statusline (opt-out via MODELSTAT_NO_STATUSLINE) ──
  // A Claude Code plugin can't register the main statusLine, so the installer
  // is the mechanism: auto-write `modelstat statusline` into the user's
  // ~/.claude/settings.json so the live per-session line (tokens · $ ·
  // taxonomy) shows at the bottom of every turn. Idempotent + composes with an
  // existing statusLine (stashes + restores it). Set MODELSTAT_NO_STATUSLINE=1
  // to skip entirely.
  if (!envFlag("MODELSTAT_NO_STATUSLINE")) {
    step("Enabling the Claude Code statusline (live tokens · $ · taxonomy)");
    try {
      const { installStatusline, claudeSettingsPath } = await import("./claude-settings.js");
      const r = installStatusline();
      if (r.kind === "installed") {
        emitEvent(opts, "statusline_installed", { preserved: r.preserved });
        ok(
          r.preserved
            ? `statusline enabled in ${claudeSettingsPath()} (your previous one was preserved)`
            : `statusline enabled in ${claudeSettingsPath()}`,
        );
      } else if (r.kind === "already") {
        emitEvent(opts, "statusline_already", {});
        ok("statusline already enabled");
      } else {
        emitEvent(opts, "statusline_failed", { error: r.message });
        warn(`couldn't enable the statusline: ${r.message}`);
      }
    } catch (e) {
      emitEvent(opts, "statusline_failed", { error: (e as Error).message });
      warn(`couldn't enable the statusline: ${(e as Error).message}`);
    }
  }

  // ── 5. Detect local AI installs + signed-in accounts ──────────
  // Discovery now RIDES the daemon's heartbeat (the standalone
  // /v1/devices/discovery endpoint is gone), so the just-installed service
  // will upsert the snapshot on its first heartbeat within seconds. We still
  // run discover() locally here — purely so the success banner can show the
  // real count and the user gets immediate confirmation — but we no longer
  // POST it from the CLI.
  step("Detecting installed AI tools and signed-in accounts");
  let discovered: { installations: number; identities: number } | null = null;
  if (state.deviceId) {
    try {
      const { discover } = await import("@modelstat/parsers");
      const d = await discover();
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

  // ── 6. Wire the modelstat MCP into your local AI tools ────────
  // The daemon is now streaming usage; wiring the MCP lets the user *ask*
  // about it from inside Claude Code / Cursor / Codex / VS Code / Zed /
  // Windsurf. We shell out to the single source of truth —
  // `npx -y @modelstat/mcp wire`, the same entry point install.sh /
  // install.ps1 call — so a bare `npx modelstat@latest` (no curl installer)
  // configures every detected tool too. Best-effort + idempotent; opt out
  // with MODELSTAT_NO_WIRE=1 (managed fleets that own their MCP config).
  let mcpWired = false;
  if (process.env.MODELSTAT_NO_WIRE) {
    emitEvent(opts, "mcp_wire_skipped", { reason: "MODELSTAT_NO_WIRE" });
  } else {
    step("Wiring the modelstat MCP into your AI tools");
    const w = await wireMcpTools(opts.json);
    mcpWired = w.ok;
    if (w.ok) {
      emitEvent(opts, "mcp_wired", {});
    } else {
      emitEvent(opts, "mcp_wire_failed", { error: w.error });
      warn(`MCP wiring skipped — run \`npx -y @modelstat/mcp wire\` later (${w.error})`);
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
    console.log(claimed ? `  Open your dashboard:` : `  Open your dashboard (no sign-up needed):`);
    console.log(`    \x1b[1;36m${claimUrl}\x1b[0m`);
    console.log();
    console.log(`  Live numbers from this terminal:`);
    console.log(
      `    \x1b[2mmodelstat status\x1b[0m  # pairing, service + sessions · tokens · cost`,
    );
    console.log(`    \x1b[2mmodelstat jobs\x1b[0m    # pipeline queue + recent activity`);
    console.log(
      `    \x1b[2mmodelstat mode\x1b[0m    # where sessions summarise (cloud/local/self-hosted)`,
    );
    console.log();
    console.log(`  Agent-friendly (for LLMs / MCPs):`);
    console.log(`    \x1b[2m${agentUrl}\x1b[0m`);
    if (mcpWired) {
      console.log(
        `    \x1b[32m✓\x1b[0m \x1b[2mMCP wired into your AI tools — ask them about your spend directly\x1b[0m`,
      );
    }
    if (!claimed) {
      console.log();
      console.log(`  Claim this device so it keeps analyzing past the free tier:`);
      console.log(`    \x1b[2m${claimUrl}/claim\x1b[0m`);
    }
    console.log(line);
    console.log();
  }

  // Try to open the claim URL in the user's browser so the tab is
  // already pointing at their data when they alt-tab away from the CLI.
  if (!opts.noBrowser) {
    const opened = tryOpenBrowser(claimUrl);
    emitEvent(opts, "browser_open_attempted", { opened });
  }

  emitEvent(opts, "done", { claim_url: claimUrl, agent_url: agentUrl, mcp_wired: mcpWired });

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
  // One-shot diagnostic enumeration of local installs + signed-in accounts.
  // Discovery is REPORTED to the server only via the daemon's heartbeat now (the
  // standalone /v1/devices/discovery endpoint is gone), so this command just
  // prints what the heartbeat would attach — it doesn't POST anything itself.
  const { discover } = await import("@modelstat/parsers");
  const out = await discover();
  console.log(`→ ${out.installations.length} installations, ${out.identities.length} identities`);
  console.log(
    "(the running daemon reports this to the server on its next heartbeat — `modelstat discover` is read-only)",
  );
}

async function cmdSync(rest: readonly string[]): Promise<void> {
  // Force-ingest specific sessions NOW — the manual / headless sibling of the
  // Claude Desktop extension's eager scan (both go through the daemon's loopback
  // control endpoint). The installed daemon already ingests every session
  // automatically; reach for this only when you need ONE session right now.
  const sessionIds = flagValues(rest, "--session");
  const file = flagValue(rest, "--file");
  if (sessionIds.length === 0 && !file) {
    console.error(
      "usage: modelstat sync --session <id> [--session <id> …] [--file <path>] [--wait]",
    );
    console.error(
      "  (the background daemon ingests everything on its own; use sync to force one session now)",
    );
    process.exit(1);
  }
  return cmdSyncSession({
    sessionIds,
    file,
    wait: rest.includes("--wait"),
    port: numericFlag(rest, "--port"),
  });
}

/**
 * Eager single-session scan — the fast path behind the `/stat` plugin and the
 * statusline's freshness. Tries a RUNNING daemon first (its summariser is
 * already resident, so the session lands in seconds via the loopback control
 * endpoint); only if no daemon is listening does it cold-load the summariser
 * and scan in-process. Refreshes the local insights cache either way (the
 * daemon does it on the control path; the standalone path does it here).
 */
async function cmdSyncSession(opts: {
  sessionIds: string[];
  file?: string;
  wait: boolean;
  port?: number;
}): Promise<void> {
  const { postControlScan } = await import("./api.js");
  const target = {
    ...(opts.sessionIds.length ? { session_ids: opts.sessionIds } : {}),
    ...(opts.file ? { file: opts.file } : {}),
  };
  const outcome = await postControlScan({ ...target, wait: opts.wait }, { port: opts.port });
  if (outcome.kind === "ok") {
    console.log(
      opts.wait
        ? "✓ daemon force-scanned the session"
        : "✓ asked the running daemon to force-scan the session",
    );
    return;
  }
  if (outcome.kind === "error") {
    console.error(`✗ daemon control scan failed (${outcome.status}): ${outcome.message}`);
    process.exit(1);
  }

  // No daemon listening — cold-scan in-process. Slower (loads the summariser)
  // but works without an installed/running daemon.
  console.log("no running daemon on the control port — scanning in-process…");
  const { preflightSummariser } = await import("./pipeline.js");
  const { label, degraded } = await preflightSummariser();
  console.log(
    degraded
      ? `[modelstat] ⚠ summariser DEGRADED — ${label}; extractive fallback, ingest continues`
      : `[modelstat] summariser preflight ok: ${label}`,
  );
  const { scanSession } = await import("./scan.js");
  const r = await scanSession({
    ...(opts.sessionIds.length ? { sessionIds: opts.sessionIds } : {}),
    ...(opts.file ? { file: opts.file } : {}),
  });
  console.log(
    `Done: ${r.filesScanned} files scanned, ${r.batchesUploaded} batches, ${r.segmentsUploaded} segments, ${r.eventsUploaded} events uploaded`,
  );
  // Refresh the local insights cache the statusline reads (the daemon does
  // this on the control path; here we do it for the standalone scan).
  if (opts.sessionIds.length > 0) {
    const { refreshSessionInsights } = await import("./insights.js");
    await refreshSessionInsights(opts.sessionIds);
  }
  try {
    const { disposeLlama } = await import("@modelstat/daemon-core/node");
    await disposeLlama();
  } catch {
    /* best-effort */
  }
}

/**
 * Manual force re-scan: wipe every file cursor + bump the local
 * processing-version stamp so the next scan re-reads all JSONLs
 * from byte 0 and re-summarises every session. Same effect as
 * shipping a new processing-version, just user-triggered (e.g. when
 * upstream changed something the daemon doesn't know about, or when
 * the user wants to backfill old sessions for a freshly-claimed
 * device).
 */
async function cmdReset(): Promise<void> {
  const { PROCESSING_VERSION } = await import("./processing-version.js");
  state.wipeCursors();
  state.setProcessingVersion(PROCESSING_VERSION);
  console.log(
    `[modelstat] cursors reset — the daemon's next scan cycle will re-read every JSONL from the start and re-summarise every session at processing version v${PROCESSING_VERSION}.`,
  );
  console.log("  If the daemon is running, kick it now with: modelstat stop && modelstat start");
}

async function cmdWatch(): Promise<void> {
  const { watchForever } = await import("./watch.js");
  await watchForever();
}

async function cmdStart(rest: string[]): Promise<void> {
  // Name the long-running daemon "modelstat agent" so `ps`/`top` and Linux GUI
  // monitors (via /proc/<pid>/comm) show it instead of a bare "node" — but ONLY
  // off macOS. On macOS, process.title also overrides what Activity Monitor and
  // `ps` display, which would hide the honest `node …/modelstat.mjs start` line
  // and defeat the `pgrep -f modelstat.mjs` the upgrade cleanup relies on. We
  // run node in place there rather than renaming the binary (a rename orphaned
  // Homebrew's libnode and bricked self-updates — see cleanupStaleLauncher), so
  // macOS shows the real "node"; only Linux gets the friendly label.
  if (platform() !== "darwin") process.title = "modelstat agent";
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
    // Remove the menu-bar tray too: its launchd agent, the running tray
    // process, and the installed .app bundle. Best-effort and macOS-only
    // (both are no-ops elsewhere) — a tray hiccup must not fail uninstall.
    try {
      uninstallTrayAutostart();
      removeTrayApp();
      console.log("✓ menu-bar tray removed");
    } catch (e) {
      console.log(`  (couldn't fully remove the tray: ${(e as Error).message})`);
    }
    // Remove our Claude Code statusLine (restoring any one we composed over).
    // Best-effort: a settings hiccup must not fail the uninstall.
    try {
      const { removeStatusline } = await import("./claude-settings.js");
      const r = removeStatusline();
      if (r.kind === "removed") {
        console.log(
          r.restored
            ? "✓ statusline removed (your previous statusLine was restored)"
            : "✓ statusline removed from Claude Code settings",
        );
      } else if (r.kind === "error") {
        console.log(`  (couldn't remove the statusline: ${r.message})`);
      }
    } catch (e) {
      console.log(`  (couldn't remove the statusline: ${(e as Error).message})`);
    }
    console.log(`  Your device pairing is still in ${state.storePath}`);
    console.log("  Run `modelstat` again to re-enable.");
  } catch (err) {
    console.error(`✗ ${(err as Error).message}`);
    process.exit(1);
  }
}

// One command for "how's my daemon, and what has it tracked?" — local
// pairing + service state, then the live usage summary (sessions · tokens ·
// cost). `--json` returns the whole object; the tray polls `status --json` and
// decodes claimed / device / claim_url / dashboard / local from it.
async function cmdStatus(args: readonly string[] = []): Promise<void> {
  const asJson = args.includes("--json");
  const s = serviceStatus();
  const paired = !!state.bearer && !!state.deviceId;
  const local = await readLocalStatus();
  const dashboard = `${state.apiUrl.replace(/\/$/, "")}/dashboard`;

  // Usage NUMBERS are driven entirely off the daemon's LOCAL heartbeat snapshot
  // (~/.modelstat/last-status.json, written by daemon.ts every status change)
  // plus a dashboard pointer for the authoritative numbers. The old
  // claim-code capability endpoint (/v1/device/:claim) was removed server-side —
  // it now returns the SPA HTML, so there's nothing to fetch for usage here.
  // (The tray's claimed/device/claim_url fields ARE refreshed from the authed
  // /v1/devices/me below, in the --json branch.)
  const stats = (local?.stats as Record<string, number | string> | undefined) ?? {};

  if (asJson) {
    // The tray decodes `claimed` / `device` / `claim_url` / `claim_code` from
    // this JSON to drive the dropdown: the claimed-vs-unclaimed UI, the device
    // hostname line ("unknown" when this is missing — the bug this restores),
    // and the "Copy claim URL" item. These USED to come from the public
    // claim-code device-view (removed server-side — it now returns SPA HTML);
    // the authed `GET /v1/devices/me` is its replacement. Best-effort: on any
    // failure (offline / transient / revoked secret) we fall back to the
    // locally-cached identity so the menu still shows the real hostname + claim
    // link. We do NOT recover the identity here — a status poll must stay
    // side-effect-free; the heartbeat loop owns 401 recovery.
    const fp = buildFingerprint();
    const device = {
      hostname: typeof fp.hostname === "string" ? fp.hostname : null,
      os_family: typeof fp.os_family === "string" ? fp.os_family : null,
      daemon_status: (local?.status as string | undefined) ?? null,
    };
    // `userEmail` is set only once /devices/me has reported the device claimed,
    // so it's a safe offline proxy for claimed-ness when the fetch below fails.
    let claimed = !!state.userEmail;
    let claimUrl = state.claimUrl;
    let claimCode = state.claimCode;
    if (paired) {
      try {
        const me = await fetchDeviceMe(state.bearer ?? "");
        claimed = me.status === "claimed";
        claimUrl = me.claim_url ?? claimUrl;
        claimCode = me.claim_code ?? claimCode;
        if (me.daemon_status) device.daemon_status = me.daemon_status;
      } catch {
        // offline / transient / revoked — keep the cached fallbacks above.
      }
    }
    process.stdout.write(
      `${JSON.stringify({
        paired,
        claimed,
        dashboard,
        device,
        claim_url: claimUrl,
        claim_code: claimCode,
        local,
        service: { running: s.running, hint: s.hint },
        pairing: paired
          ? {
              paired: true,
              user: state.userEmail ?? null,
              device: state.deviceId,
              uuid: state.deviceUuid ?? null,
            }
          : { paired: false },
        auto_update: { enabled: autoUpdateEnabled(), pinned_by_env: autoUpdatePinnedByEnv() },
        // Active summariser mode (the tray reads this to show + switch it).
        // Mirrors `modelstat mode --json`: endpoint fields only for self-hosted.
        summarizer: {
          mode: state.summarizerMode,
          ...(state.summarizerMode === "self-hosted" ? state.selfHosted : {}),
          env_override: state.summarizerModeIsEnvOverridden,
        },
        api: state.apiUrl,
        logs: logsDir(),
        state: state.storePath,
      })}\n`,
    );
    return;
  }

  // ── local: pairing + service ──────────────────────────────────────
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
  const sm = state.summarizerMode;
  const smEndpoint =
    sm === "self-hosted" && state.selfHosted.url ? ` (${state.selfHosted.url})` : "";
  const smEnv = state.summarizerModeIsEnvOverridden ? " (env override)" : "";
  console.log(`summariser: ${sm}${smEndpoint}${smEnv} — change with \`modelstat mode\``);
  console.log(
    `auto-update: ${autoUpdateEnabled() ? "on" : "off"}${autoUpdatePinnedByEnv() ? " (pinned by env)" : ""}`,
  );
  const upd = local?.update as { verdict?: string; latest?: string | null } | null | undefined;
  if (upd?.verdict && upd.verdict !== "ok") {
    const what = upd.verdict === "upgrade_required" ? "REQUIRED" : "available";
    console.log(`update:  ${what} — latest ${upd.latest ?? "?"} (run \`modelstat upgrade\`)`);
  }

  // ── live usage: from the local heartbeat snapshot + dashboard pointer ──
  console.log("");
  if (!paired) {
    console.log("usage:   not paired yet — run `npx modelstat@latest`");
    return;
  }
  console.log("usage:   full numbers in your dashboard:");
  console.log(`  ${dashboard}`);
  if (local) {
    const phase = local.status as string | undefined;
    const message = local.message as string | null | undefined;
    if (phase) console.log(`  phase: ${phase}${message ? ` — ${message}` : ""}`);
    for (const [k, v] of Object.entries(stats)) console.log(`  ${k}: ${v}`);
  } else {
    console.log("  (no local heartbeat yet — is the daemon running?)");
  }
}

/** Show or change the daemon auto-update setting: on | off | toggle | status. */
function cmdAutoUpdate(args: readonly string[]): void {
  const sub = (args[0] ?? "").toLowerCase();
  if (sub === "on" || sub === "enable") {
    setStoredAutoUpdate(true);
  } else if (sub === "off" || sub === "disable") {
    setStoredAutoUpdate(false);
  } else if (sub === "toggle") {
    setStoredAutoUpdate(!storedAutoUpdate());
  } else if (sub !== "" && sub !== "status") {
    console.log("usage: modelstat autoupdate [on|off|toggle|status]");
    return;
  }
  console.log(`auto-update: ${autoUpdateEnabled() ? "on" : "off"}`);
  if (autoUpdatePinnedByEnv()) {
    console.log("  (pinned by MODELSTAT_AUTO_UPDATE env — the stored toggle is ignored)");
  }
}

/** Manually upgrade to the latest published version now (ignores the
 * auto-update setting). The tray's "Update now" item shells this. */
function cmdUpgrade(): void {
  console.log("upgrading modelstat to the latest published version…");
  const r = runUpgrade();
  if (r.started) {
    console.log(
      "  started `npm install -g modelstat@latest` — the service restarts on the new build.",
    );
  } else {
    console.log(`  couldn't start the upgrade: ${r.reason}`);
    console.log("  upgrade manually: npm install -g modelstat@latest");
  }
}

/** Best-effort read of the daemon's heartbeat mirror. Returns the
 * parsed object or null if the file isn't there / can't be parsed. */
async function readLocalStatus(): Promise<Record<string, unknown> | null> {
  try {
    const { readFile } = await import("node:fs/promises");
    const { homePath } = await import("./paths.js");
    const p = homePath("last-status.json");
    const txt = await readFile(p, "utf8");
    return JSON.parse(txt) as Record<string, unknown>;
  } catch {
    return null;
  }
}

/** Background-pipeline view. The authoritative job queue + ledger live in the
 * dashboard (the old claim-code capability endpoints were removed server-side —
 * they now return the SPA HTML). Locally we surface the daemon's own pipeline
 * activity from its heartbeat snapshot (phase + queue depth + upload stats) and
 * point at the dashboard for the full picture. */
async function cmdJobs(args: readonly string[]): Promise<void> {
  const asJson = args.includes("--json");
  const paired = !!state.bearer && !!state.deviceId;
  const dashboard = `${state.apiUrl.replace(/\/$/, "")}/dashboard/jobs`;
  if (!paired) {
    if (asJson) {
      process.stdout.write(`${JSON.stringify({ paired: false, reason: "not_paired" })}\n`);
    } else {
      console.log("not paired yet — run `npx modelstat@latest` first");
    }
    return;
  }
  const local = await readLocalStatus();
  const stats = (local?.stats as Record<string, number | string> | undefined) ?? {};
  const phase = (local?.status as string | undefined) ?? null;
  const queue = Number(local?.queue_size ?? 0);
  if (asJson) {
    process.stdout.write(
      `${JSON.stringify({ paired: true, dashboard, phase, queue_size: queue, stats })}\n`,
    );
    return;
  }
  console.log("jobs:    full job queue + ledger in your dashboard:");
  console.log(`  ${dashboard}`);
  console.log("");
  console.log("local pipeline (this device):");
  if (local) {
    const message = local.message as string | null | undefined;
    if (phase) console.log(`  phase: ${phase}${message ? ` — ${message}` : ""}`);
    console.log(`  queue: ${queue}`);
    for (const [k, v] of Object.entries(stats)) console.log(`  ${k}: ${v}`);
  } else {
    console.log("  (no local heartbeat yet — is the daemon running?)");
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

/**
 * `modelstat mode [cloud|local|self-hosted]` — show or change where sessions
 * get summarised (redaction always stays on-device). With no argument it prints
 * the current mode; with one it persists the new mode (prompting for /
 * validating a self-hosted endpoint), pulls the local model when switching TO
 * local, and refreshes the background service so the running daemon reloads.
 */
async function cmdMode(argv: readonly string[]): Promise<void> {
  const json = argv.includes("--json");
  // A bare positional (not a --flag) is the mode; --mode is also accepted.
  const positional = argv.find((a) => !a.startsWith("-"));
  const requested = flagValue(argv, "--mode") ?? positional;

  // No argument → report the current mode + endpoint.
  if (!requested) {
    const mode = state.summarizerMode;
    if (json) {
      process.stdout.write(
        `${JSON.stringify({
          mode,
          ...(mode === "self-hosted" ? state.selfHosted : {}),
          env_override: state.summarizerModeIsEnvOverridden,
        })}\n`,
      );
      return;
    }
    console.log(`summariser mode: ${mode}`);
    console.log(`  ${MODE_INFO[mode].label}`);
    console.log(`  resource: ${MODE_INFO[mode].resource}`);
    console.log(`  privacy:  ${MODE_INFO[mode].privacy}`);
    if (mode === "self-hosted") {
      const sh = state.selfHosted;
      console.log(`  endpoint: ${sh.url || "(unset)"}   model: ${sh.model || "(unset)"}`);
    }
    if (state.summarizerModeIsEnvOverridden) {
      console.log("  note: MODELSTAT_SUMMARIZER_MODE is set and overrides the stored value.");
    }
    console.log("change it: modelstat mode <cloud|local|self-hosted> [--url <URL> --model <ID>]");
    return;
  }

  let mode: SummarizerMode;
  try {
    mode = await resolveAndPersistMode({
      requested,
      url: flagValue(argv, "--url"),
      model: flagValue(argv, "--model"),
      interactive: !json && process.stdin.isTTY === true,
    });
  } catch (e) {
    console.error(`couldn't set mode: ${(e as Error).message}`);
    process.exit(1);
    return;
  }
  console.log(`✓ summariser mode set to ${mode}`);
  if (state.summarizerModeIsEnvOverridden) {
    console.warn(
      `⚠ MODELSTAT_SUMMARIZER_MODE is set — the daemon will use "${state.summarizerMode}" until you unset it`,
    );
  }

  // Switching TO local needs the model — pull it now so the next scan is
  // instant (best-effort; otherwise the daemon downloads it lazily on first scan).
  if (mode === "local") {
    try {
      const { ensureLlamaModel, defaultLlamaConfig } = await import("@modelstat/daemon-core/node");
      console.log("preparing local summariser model (downloads on first use)…");
      await ensureLlamaModel(defaultLlamaConfig());
      console.log("✓ summariser model on disk");
    } catch (e) {
      console.warn(
        `couldn't pre-download the model (${(e as Error).message}); it downloads on first scan`,
      );
    }
  }

  // Re-stage the native runtime for the new mode + bounce the service so the
  // running daemon reloads. Only when there's an installed daemon to refresh.
  if (state.bearer) {
    try {
      const svc = installService();
      console.log(`✓ background service refreshed (${svc.path})`);
    } catch (e) {
      console.warn(
        `couldn't refresh the service (${(e as Error).message}) — restart it by re-running \`modelstat\``,
      );
    }
  } else {
    console.log("run `modelstat` to install the background service with this mode.");
  }
}

function parseConnectOpts(argv: readonly string[]): ConnectOpts {
  return {
    json: argv.includes("--json"),
    noBrowser: argv.includes("--no-browser"),
    fresh: argv.includes("--fresh"),
    yes: argv.includes("--yes") || argv.includes("-y"),
    mode: flagValue(argv, "--mode"),
    selfHostedUrl: flagValue(argv, "--url"),
    selfHostedModel: flagValue(argv, "--model"),
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
    case "_install-service": {
      // Internal: stage the bundle + native runtime AND (re)install the managed
      // launchd/systemd service in ONE idempotent step (installService does
      // both — see service.ts). The result is an ALWAYS-ON daemon (RunAtLoad +
      // KeepAlive on macOS, Restart=always on Linux) running the freshly-staged
      // bundle — NOT a detached hand-spawned process. The upgrade postinstall
      // calls this so every upgrade leaves a properly-managed service, and a
      // previously hand-spawned daemon gets converted to a managed one.
      const svc = installService();
      console.log(`✓ managed background service installed + started (${svc.path})`);
      // If a menu-bar tray was already installed, refresh it to this
      // version and re-arm its autostart agent so an auto-update / upgrade
      // leaves the NEW tray running. No-op when no tray was installed.
      if (refreshTrayIfInstalled()) console.log("✓ menu-bar tray refreshed to this version");
      return;
    }
    case "_daemon-health": {
      // Internal: one-line JSON verdict for supervisors (the macOS
      // tray) deciding whether to adopt the live daemon, spawn a
      // fresh one, or force-replace a wedged one — see supervise.ts.
      // Never throws: a broken probe must not strand the supervisor,
      // so any failure degrades to "spawn".
      try {
        console.log(JSON.stringify(daemonHealth({ myDaemonVersion: DAEMON_VERSION })));
      } catch (e) {
        console.log(JSON.stringify({ decision: "spawn", error: (e as Error).message }));
      }
      return;
    }

    // ── Diagnostics / dev one-shots ────────────────────────────
    case "status":
      return cmdStatus(rest);
    case "jobs":
      return cmdJobs(rest);
    case "paths":
      cmdPaths(rest);
      return;
    case "token":
      cmdToken(rest);
      return;
    case "mode":
      // Show or change where sessions get summarised: cloud | local |
      // self-hosted (redaction always stays on-device). See cmdMode.
      return cmdMode(rest);
    case "discover":
      return cmdDiscover();
    case "sync":
      return cmdSync(rest);
    case "statusline": {
      // Claude Code's always-on status line. Reads its stdin JSON, prints one
      // compact line from the LOCAL insights cache — never blocks, never hits
      // the network, never throws (a crashing statusline wedges the prompt).
      // Dynamically imported so the common CLI paths don't pay for it.
      const { runStatusline } = await import("./statusline.js");
      await runStatusline();
      return;
    }
    case "reset":
      return cmdReset();
    case "watch":
      return cmdWatch();
    case "self-register":
      return cmdSelfRegister();
    case "await-claim":
      return cmdAwaitClaim();
    case "upgrade":
      // Upgrade to the latest version now (ignores the auto-update setting).
      cmdUpgrade();
      return;
    case "autoupdate":
      // Show or change the auto-update setting: on | off | toggle | status.
      cmdAutoUpdate(rest);
      return;
    default:
      console.log("usage:");
      console.log(
        "  npx modelstat@latest                — install or upgrade. Registers the device, installs the background service, wires the MCP into your AI tools, exits.",
      );
      console.log(
        "                                        flags: --json, --no-browser, --fresh, -y, --mode <cloud|local|self-hosted> [--url --model]",
      );
      console.log(
        "  npx modelstat@latest remove         — stop and uninstall the background service. Keeps your identity.",
      );
      console.log(
        "  npx modelstat@latest reinstall      — alias for the default. Useful when you want to be explicit.",
      );
      console.log();
      console.log("Diagnostics:");
      console.log(
        "  npx modelstat@latest status         — pairing, service + live usage: sessions · tokens · cost (--json)",
      );
      console.log("  npx modelstat@latest upgrade        — update to the latest version now");
      console.log("  npx modelstat@latest autoupdate     — show/set auto-update: on|off|toggle");
      console.log(
        "  npx modelstat@latest jobs           — pipeline queue + recent processing ledger (--json)",
      );
      console.log(
        "  npx modelstat@latest paths          — print state file + log dir + api URL (--json)",
      );
      console.log(
        "  npx modelstat@latest token          — print the device token for hosted MCP / API access (--json)",
      );
      console.log(
        "  npx modelstat@latest mode           — show/set where sessions summarise: cloud|local|self-hosted",
      );
      console.log(
        "  modelstat statusline                — Claude Code status line (reads stdin JSON; auto-enabled on install)",
      );
      console.log();
      console.log("Dev / one-shots:");
      console.log(
        "  npx modelstat@latest sync --session <id>  — force-ingest ONE session now (--file <path>, --wait)",
      );
      console.log(
        "  npx modelstat@latest reset          — reset file cursors so the daemon re-reads & re-summarises everything",
      );
      console.log(
        "  npx modelstat@latest watch          — foreground watcher (chokidar + periodic backstop; no service)",
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
