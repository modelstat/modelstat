/**
 * Resolve the modelstat daemon state. Single source of truth: the daemon
 * CLI's `modelstat paths --json` output, which prints the real Conf
 * store path regardless of whether the CLI was installed via npm or
 * Homebrew. That guarantees MCP reads from the same file the daemon
 * writes to — no duplicate device identity, same bearer.
 *
 * Fallback chain (most trusted first):
 *   1. `modelstat paths --json` on PATH
 *   2. MODELSTAT_STATE_FILE env var (explicit override)
 *   3. Heuristic: reproduce Conf's default path for projectName
 *      "modelstat-daemon" on macOS / Linux
 *
 * If all fall through to "no bearer", tools surface a friendly
 * "run npx modelstat@latest" error.
 */
import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir, platform } from "node:os";
import { dirname, join } from "node:path";

import type { McpToolDecl } from "./api.js";

export type State = {
  bearer?: string;
  deviceId?: string;
  deviceUuid?: string;
  userEmail?: string;
  apiUrl: string;
  statePath: string;
};

/** Compute Conf's default path for the daemon. Mirrors env-paths logic
 * so we don't take a runtime dep. If Conf's algorithm ever changes,
 * prefer `modelstat paths --json` output (which calls the real Conf). */
function heuristicStatePath(): string {
  const name = "modelstat-daemon-nodejs";
  const home = homedir();
  if (platform() === "darwin") {
    return join(home, "Library", "Preferences", name, "config.json");
  }
  // Linux (and anything else XDG-ish).
  const xdgConfig = process.env.XDG_CONFIG_HOME;
  const base = xdgConfig && xdgConfig.length > 0 ? xdgConfig : join(home, ".config");
  return join(base, name, "config.json");
}

/** Try the CLI first — it's authoritative. Swallow errors and fall
 * through; callers have a heuristic backup. */
function askCliForState(): { statePath: string; apiUrl?: string } | null {
  try {
    const out = execFileSync("modelstat", ["paths", "--json"], {
      stdio: ["ignore", "pipe", "ignore"],
      timeout: 2000,
      encoding: "utf8",
    });
    const parsed = JSON.parse(out) as { state?: string; api?: string };
    if (parsed.state) return { statePath: parsed.state, apiUrl: parsed.api };
  } catch {
    // modelstat not on PATH, or an old version without `paths`. Fine —
    // we'll use the heuristic path below.
  }
  return null;
}

export function loadState(): State {
  const envApi = process.env.MODELSTAT_API_URL ?? process.env.DAEMON_API_URL;
  const cliInfo = askCliForState();
  const overridePath = process.env.MODELSTAT_STATE_FILE;
  const statePath = overridePath ?? cliInfo?.statePath ?? heuristicStatePath();

  let stored: Partial<State> = {};
  try {
    const raw = readFileSync(statePath, "utf8");
    stored = JSON.parse(raw) as Partial<State>;
  } catch {
    // File doesn't exist yet — user hasn't paired. That's a normal
    // first-run state; callers render "not paired" to the user.
  }

  // Field names in Conf's store match the AgentState interface in
  // apps/daemon/src/config.ts: bearerToken, deviceId, deviceUuid,
  // userEmail, apiUrl (may be "" post-0.0.8 migration).
  const rec = stored as Record<string, unknown>;
  return {
    bearer: typeof rec.bearerToken === "string" ? rec.bearerToken : undefined,
    deviceId: typeof rec.deviceId === "string" ? rec.deviceId : undefined,
    deviceUuid: typeof rec.deviceUuid === "string" ? rec.deviceUuid : undefined,
    userEmail: typeof rec.userEmail === "string" ? rec.userEmail : undefined,
    apiUrl:
      envApi
      ?? cliInfo?.apiUrl
      ?? (typeof rec.apiUrl === "string" && rec.apiUrl && rec.apiUrl !== "http://localhost:3010"
        ? rec.apiUrl
        : "https://modelstat.ai"),
    statePath,
  };
}

/** Describe the machine the MCP server is running on — shown in
 * `get_device_status` so users can sanity-check they're querying the
 * right laptop. */
export function machineDescriptor(): { platform: string; hostname: string } {
  return {
    platform: platform(),
    hostname: process.env.HOSTNAME ?? "localhost",
  };
}

// ─── tools-catalog cache ──────────────────────────────────────────
// Persist the last-successful `/v1/mcp/tools` response beside the
// pairing state. Read only when the live fetch fails, so staleness is
// bounded by "backend currently unreachable".

export function toolsCachePath(state: State): string {
  return join(dirname(state.statePath), "mcp-tools-cache.json");
}

export function readToolsCache(state: State): { tools: McpToolDecl[] } | null {
  try {
    const raw = readFileSync(toolsCachePath(state), "utf8");
    const parsed = JSON.parse(raw) as { tools?: unknown };
    if (Array.isArray(parsed.tools)) {
      return { tools: parsed.tools as McpToolDecl[] };
    }
  } catch {
    // absent or malformed — treat as no cache
  }
  return null;
}

export function writeToolsCache(
  state: State,
  payload: { tools: McpToolDecl[] },
): void {
  const target = toolsCachePath(state);
  const tmp = `${target}.tmp-${process.pid}`;
  try {
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(tmp, JSON.stringify(payload), { encoding: "utf8", mode: 0o600 });
    renameSync(tmp, target);
  } catch {
    // Cache failure must never break the happy path — the live fetch
    // already succeeded, so the client gets the right answer this
    // session. Best-effort cleanup of the temp file.
    try {
      unlinkSync(tmp);
    } catch {
      /* ignore */
    }
  }
}
