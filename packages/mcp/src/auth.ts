/**
 * Browser claim flow (PR-5b) — make the MCP work with NO daemon installed.
 *
 * When no local bearer is found (no daemon, no prior MCP claim), we register a
 * device against the modelstat server, open its claim URL in the user's
 * browser (they're signed into the dashboard → the claim binds the device to
 * their org), poll until claimed, and persist the bearer to
 * ~/.modelstat/mcp-auth.json (see state.ts `persistMcpAuth`). The local-daemon
 * bearer remains the fast path; this is the fallback other MCPs do with an
 * OAuth popup — here it reuses modelstat's existing device claim flow.
 *
 * stdio discipline: stdout is the MCP transport, so every human-facing line
 * goes to STDERR. We never block forever — the poll is capped.
 */
import { spawn } from "node:child_process";
import { hostname, platform, release } from "node:os";
import { persistMcpAuth, type State } from "./state.js";

/** Server self-register response (unauth). Mirrors the daemon's proven
 * contract (apps/daemon/src/api.ts SelfRegisterResponse). */
interface SelfRegisterResponse {
  device_uuid: string;
  device_id: string;
  device_secret: string;
  claim_code: string;
  claim_url: string;
  status: string;
}

/** `GET /v1/devices/me` — we only need the claimed signal. */
interface DeviceMeResponse {
  status?: string;
  user_id?: string | null;
}

function err(line: string): void {
  process.stderr.write(`modelstat-mcp: ${line}\n`);
}

/** RFC4122-ish v4 uuid — the MCP's device is a distinct logical device, so a
 * random id is correct (no need to mirror the daemon's machine-derived one). */
function randomUuid(): string {
  // Node 20 has global crypto.randomUUID.
  return globalThis.crypto.randomUUID();
}

/** Best-effort open a URL in the default browser; false if we couldn't spawn an
 * opener (headless / no DE) so the caller prints the URL to copy. */
function openBrowser(url: string): boolean {
  const p = platform();
  const cmd = p === "darwin" ? "open" : p === "win32" ? "cmd" : "xdg-open";
  const args = p === "win32" ? ["/c", "start", "", url] : [url];
  try {
    const child = spawn(cmd, args, { stdio: "ignore", detached: true });
    child.unref();
    return true;
  } catch {
    return false;
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/**
 * Run the full self-register → claim → poll flow. Returns a State with a bearer
 * on success, or the unchanged (bearer-less) state if the user didn't claim in
 * time / something failed (the caller then surfaces a friendly "not paired"
 * error). Persists the bearer to mcp-auth.json on success.
 *
 * Disabled in non-interactive contexts (no TTY on stderr and not explicitly
 * allowed) so a headless MCP host doesn't spawn browser windows / pile up
 * unclaimed device rows; set MODELSTAT_MCP_BROWSER_AUTH=1 to force it.
 */
export async function browserClaim(state: State): Promise<State> {
  if (!shouldAttempt()) {
    err(
      "not paired and no interactive terminal — set MODELSTAT_MCP_BROWSER_AUTH=1 to claim via browser, or install the daemon (npx modelstat@latest).",
    );
    return state;
  }

  let reg: SelfRegisterResponse;
  try {
    reg = await selfRegister(state.apiUrl);
  } catch (e) {
    err(`device self-register failed: ${(e as Error).message}`);
    return state;
  }

  err("");
  err("modelstat needs to connect this MCP to your account (one-time).");
  err(`Opening: ${reg.claim_url}`);
  err("If your browser didn't open, paste that URL and sign in to claim the device.");
  err("");
  openBrowser(reg.claim_url);

  // Poll devices/me with the new bearer until the user claims it in the
  // browser. Capped so we never hang the MCP host forever.
  const deadline = Date.now() + CLAIM_TIMEOUT_MS;
  while (Date.now() < deadline) {
    await sleep(POLL_INTERVAL_MS);
    let me: DeviceMeResponse | null = null;
    try {
      me = await fetchDeviceMe(state.apiUrl, reg.device_secret);
    } catch {
      // transient — keep polling
    }
    if (me && (me.status === "claimed" || (me.user_id && me.user_id.length > 0))) {
      persistMcpAuth({
        bearer: reg.device_secret,
        deviceId: reg.device_id,
        deviceUuid: reg.device_uuid,
      });
      err("✓ connected — this MCP is now linked to your modelstat account.");
      return {
        ...state,
        bearer: reg.device_secret,
        deviceId: reg.device_id,
        deviceUuid: reg.device_uuid,
        source: "mcp-auth",
      };
    }
  }

  err(
    "timed out waiting for the device to be claimed. Re-run your request after signing in, " +
      "or install the daemon: npx modelstat@latest.",
  );
  return state;
}

const POLL_INTERVAL_MS = 2500;
const CLAIM_TIMEOUT_MS = 3 * 60_000; // 3 min — plenty to sign in + claim

function shouldAttempt(): boolean {
  if (process.env.MODELSTAT_MCP_BROWSER_AUTH === "0") return false;
  if (process.env.MODELSTAT_MCP_BROWSER_AUTH === "1") return true;
  // Default: only when a human can see the stderr prompts.
  return process.stderr.isTTY === true;
}

async function selfRegister(apiUrl: string): Promise<SelfRegisterResponse> {
  // The ONE register door is POST /v1/tokens (it folds device self-register);
  // it returns the agentic `{ data: … }` envelope and a `ds_live_` secret.
  const res = await fetch(new URL("/v1/tokens", apiUrl), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      device_uuid: randomUuid(),
      fingerprint: {
        source: "mcp",
        hostname: hostname(),
        platform: platform(),
        release: release(),
      },
    }),
  });
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}: ${(await res.text().catch(() => "")).slice(0, 200)}`);
  }
  const body = (await res.json()) as { data: SelfRegisterResponse };
  return body.data;
}

async function fetchDeviceMe(apiUrl: string, secret: string): Promise<DeviceMeResponse> {
  const res = await fetch(new URL("/v1/devices/me", apiUrl), {
    headers: { authorization: `Bearer ${secret}` },
  });
  if (!res.ok) throw new Error(`devices/me ${res.status}`);
  const body = (await res.json()) as { data: DeviceMeResponse };
  return body.data;
}
