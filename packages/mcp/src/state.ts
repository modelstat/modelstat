/**
 * Resolve the modelstat MCP server's auth + endpoint.
 *
 * Auth resolution (most trusted first):
 *   1. A running daemon's token — `modelstat token --json` (authoritative; the
 *      daemon resolves its own bearer + apiUrl). Fast path when the daemon CLI
 *      is installed.
 *   2. The daemon's identity file directly — `~/.modelstat/identity.json`
 *      (`bearerToken`), located via `modelstat paths --json` or the default
 *      home. Covers "daemon installed but not on PATH for this shell".
 *   3. Our OWN persisted MCP bearer — `~/.modelstat/mcp-auth.json`, written by
 *      the browser claim flow (see auth.ts). This is what makes the MCP work
 *      with NO daemon installed.
 *
 * If none resolve, the bridge runs the browser claim flow on first use
 * (auth.ts), then persists to (3). Env overrides:
 *   - MODELSTAT_TOKEN / MODELSTAT_API_URL (or DAEMON_API_URL) force the bearer
 *     / endpoint (CI, self-host).
 *   - MODELSTAT_STATE_FILE points at an explicit daemon identity file.
 *   - MODELSTAT_HOME relocates the daemon home (matches the daemon's own).
 */
import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import type { McpToolDecl } from "./api.js";

export type State = {
  bearer?: string;
  deviceId?: string;
  deviceUuid?: string;
  userEmail?: string;
  apiUrl: string;
  /** Where our own MCP bearer is / would be persisted (browser claim flow). */
  authPath: string;
  /** How the bearer was resolved — for diagnostics + deciding whether to
   * persist a freshly-claimed one. */
  source: "env" | "daemon-token" | "daemon-identity" | "mcp-auth" | "none";
};

const PROD_API = "https://modelstat.ai";
const LEGACY_LOCALHOST = "http://localhost:3010";

/** The daemon's single home dir — mirrors apps/daemon/src/paths.ts
 * `modelstatHome()` (MODELSTAT_HOME override, else ~/.modelstat) so the MCP
 * reads/writes the same place the daemon does. */
export function modelstatHome(): string {
  const override = process.env.MODELSTAT_HOME?.trim();
  return override && override.length > 0 ? override : join(homedir(), ".modelstat");
}

/** Path to our own persisted MCP bearer (browser claim flow output). */
export function mcpAuthPath(): string {
  return join(modelstatHome(), "mcp-auth.json");
}

interface CliPaths {
  identity?: string;
  state?: string;
  api?: string;
}

/** Ask the daemon CLI where its files live + which API it targets. Authoritative
 * when present; swallow everything (CLI absent / old version) and fall back. */
function askCliPaths(): CliPaths | null {
  try {
    const out = execFileSync("modelstat", ["paths", "--json"], {
      stdio: ["ignore", "pipe", "ignore"],
      timeout: 2000,
      encoding: "utf8",
    });
    return JSON.parse(out) as CliPaths;
  } catch {
    return null;
  }
}

/** Fast path: the daemon prints its own resolved bearer + apiUrl. */
function askCliToken(): { token: string; api?: string } | null {
  try {
    const out = execFileSync("modelstat", ["token", "--json"], {
      stdio: ["ignore", "pipe", "ignore"],
      timeout: 2000,
      encoding: "utf8",
    });
    const parsed = JSON.parse(out) as { token?: string; api?: string };
    if (parsed.token) return { token: parsed.token, api: parsed.api };
  } catch {
    // Not paired / CLI absent — fall through to file reads.
  }
  return null;
}

/** Read a daemon identity.json (bearer lives here, NOT in state.json). */
function readIdentityFile(path: string): { bearer?: string; deviceId?: string; deviceUuid?: string } {
  try {
    const rec = JSON.parse(readFileSync(path, "utf8")) as Record<string, unknown>;
    return {
      bearer: typeof rec.bearerToken === "string" ? rec.bearerToken : undefined,
      deviceId: typeof rec.deviceId === "string" ? rec.deviceId : undefined,
      deviceUuid: typeof rec.deviceUuid === "string" ? rec.deviceUuid : undefined,
    };
  } catch {
    return {};
  }
}

/** Read our own persisted MCP bearer. */
function readMcpAuth(): { bearer?: string; deviceId?: string; deviceUuid?: string } {
  return readIdentityFile(mcpAuthPath()); // same field shape (bearerToken, deviceId, …)
}

/** Resolve everything except the (optional) interactive browser claim, which
 * the caller runs only when `source === "none"`. */
export function loadState(): State {
  const envApi = process.env.MODELSTAT_API_URL ?? process.env.DAEMON_API_URL;
  const envToken = process.env.MODELSTAT_TOKEN;
  const authPath = mcpAuthPath();

  const normaliseApi = (api: string | undefined): string | undefined =>
    api && api.length > 0 && api !== LEGACY_LOCALHOST ? api : undefined;

  // 0. Explicit env bearer wins (CI / self-host).
  if (envToken) {
    return {
      bearer: envToken,
      apiUrl: normaliseApi(envApi) ?? PROD_API,
      authPath,
      source: "env",
    };
  }

  // 1. Running daemon's own token (authoritative).
  const tok = askCliToken();
  if (tok) {
    return {
      bearer: tok.token,
      apiUrl: normaliseApi(envApi) ?? normaliseApi(tok.api) ?? PROD_API,
      authPath,
      source: "daemon-token",
    };
  }

  // 2. Daemon identity file directly.
  const cli = askCliPaths();
  const identityPath =
    process.env.MODELSTAT_STATE_FILE ?? cli?.identity ?? join(modelstatHome(), "identity.json");
  const fromIdentity = readIdentityFile(identityPath);
  if (fromIdentity.bearer) {
    return {
      bearer: fromIdentity.bearer,
      deviceId: fromIdentity.deviceId,
      deviceUuid: fromIdentity.deviceUuid,
      apiUrl: normaliseApi(envApi) ?? normaliseApi(cli?.api) ?? PROD_API,
      authPath,
      source: "daemon-identity",
    };
  }

  // 3. Our own persisted MCP bearer (browser claim flow output).
  const fromMcp = readMcpAuth();
  if (fromMcp.bearer) {
    return {
      bearer: fromMcp.bearer,
      deviceId: fromMcp.deviceId,
      deviceUuid: fromMcp.deviceUuid,
      apiUrl: normaliseApi(envApi) ?? PROD_API,
      authPath,
      source: "mcp-auth",
    };
  }

  // Nothing — caller may run the browser claim flow.
  return { apiUrl: normaliseApi(envApi) ?? PROD_API, authPath, source: "none" };
}

/** Persist a bearer obtained via the browser claim flow to mcp-auth.json
 * (0600). Uses the daemon identity field names so a future daemon install can
 * read it too. */
export function persistMcpAuth(auth: {
  bearer: string;
  deviceId?: string;
  deviceUuid?: string;
}): void {
  const target = mcpAuthPath();
  const tmp = `${target}.tmp-${process.pid}`;
  const payload = {
    bearerToken: auth.bearer,
    deviceId: auth.deviceId ?? null,
    deviceUuid: auth.deviceUuid ?? null,
    createdAt: new Date().toISOString(),
    source: "mcp-browser-claim",
  };
  try {
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(tmp, JSON.stringify(payload), { encoding: "utf8", mode: 0o600 });
    renameSync(tmp, target);
  } catch {
    try {
      unlinkSync(tmp);
    } catch {
      /* ignore */
    }
  }
}

// ─── tools-catalog cache ──────────────────────────────────────────
// Persist the last-successful `/v1/mcp/tools` response beside our auth file.
// Read only when the live fetch fails, so staleness is bounded by "backend
// currently unreachable".

export function toolsCachePath(state: State): string {
  return join(dirname(state.authPath), "mcp-tools-cache.json");
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

export function writeToolsCache(state: State, payload: { tools: McpToolDecl[] }): void {
  const target = toolsCachePath(state);
  const tmp = `${target}.tmp-${process.pid}`;
  try {
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(tmp, JSON.stringify(payload), { encoding: "utf8", mode: 0o600 });
    renameSync(tmp, target);
  } catch {
    // Cache failure must never break the happy path — the live fetch already
    // succeeded, so the client gets the right answer this session.
    try {
      unlinkSync(tmp);
    } catch {
      /* ignore */
    }
  }
}
