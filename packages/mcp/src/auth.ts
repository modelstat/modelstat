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
import { deviceIdentity } from "./device-id.js";
import { persistMcpAuth, type State } from "./state.js";

/** Response from the ONE register door, `POST /v1/tokens`. Mirrors the daemon's
 * contract (apps/daemon/src/api.ts SelfRegisterResponse). `claim_code` /
 * `claim_url` are null once the device is already claimed (a re-registered,
 * known machine) — the server returns no fresh claim handle for it. */
interface SelfRegisterResponse {
  device_id: string;
  device_uuid: string;
  /** Opaque device secret — `ds_live_<…>`. Stored + sent verbatim as a
   * Bearer; we make no assumption about its format. */
  device_secret: string;
  secret_prefix: string;
  claim_code: string | null;
  claim_url: string | null;
  status: "unclaimed" | "claimed";
  user_id: string | null;
  /** Set when the server reused an existing row (same machine_id) and minted a
   * fresh secret instead of creating a new device. */
  re_registered?: boolean;
}

/** `GET /v1/devices/me` — we only need the claimed signal. The full payload
 * also carries `last_active_at` (renamed from `last_seen_at`), which the MCP
 * does not consume. */
interface DeviceMeResponse {
  status?: "unclaimed" | "claimed";
  user_id?: string | null;
  last_active_at?: string | null;
}

/** Every device-endpoint payload is wrapped in `{ data: … }`. Unwrap it,
 * tolerating a bare body (older server / test stub) so we never crash. Mirrors
 * apps/daemon/src/api.ts `unwrapData`. */
function unwrapData<T>(body: unknown): T {
  if (body && typeof body === "object" && "data" in (body as Record<string, unknown>)) {
    return (body as { data: T }).data;
  }
  return body as T;
}

function err(line: string): void {
  process.stderr.write(`modelstat-mcp: ${line}\n`);
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
    err(`device registration failed: ${(e as Error).message}`);
    return state;
  }

  // Already claimed: the server recognised this machine (machine_id dedupe) and
  // minted a fresh secret for the existing, already-bound device. No browser
  // round-trip is needed — the secret is immediately usable. Persist and return.
  if (reg.status === "claimed") {
    return connected(state, reg, "this MCP is already linked to your modelstat account.");
  }

  // Unclaimed but no claim URL — the server gave us nothing to open, so there's
  // no way to pair from here. Surface the daemon as the fallback.
  if (!reg.claim_url) {
    err(
      "device registered but the server returned no claim URL — cannot pair from the MCP. " +
        "Install the daemon instead: npx modelstat@latest.",
    );
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
      return connected(state, reg, "this MCP is now linked to your modelstat account.");
    }
  }

  err(
    "timed out waiting for the device to be claimed. Re-run your request after signing in, " +
      "or install the daemon: npx modelstat@latest.",
  );
  return state;
}

/** Persist the freshly-registered bearer and return a connected State. Shared by
 * the already-claimed fast path and the post-poll success path. */
function connected(state: State, reg: SelfRegisterResponse, tail: string): State {
  persistMcpAuth({
    bearer: reg.device_secret,
    deviceId: reg.device_id,
    deviceUuid: reg.device_uuid,
  });
  err(`✓ connected — ${tail}`);
  return {
    ...state,
    bearer: reg.device_secret,
    deviceId: reg.device_id,
    deviceUuid: reg.device_uuid,
    source: "mcp-auth",
  };
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
  // it returns the agentic `{ data: … }` envelope and a `ds_live_` secret. We
  // send a STABLE `fingerprint.machine_id` (and matching `device_uuid`) so the
  // server dedupes repeated registrations of this machine onto one device row
  // instead of leaking a fresh unclaimed device on every claim attempt.
  const id = deviceIdentity();
  const res = await fetch(new URL("/v1/tokens", apiUrl), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      device_uuid: id.deviceUuid,
      fingerprint: {
        source: "mcp",
        hostname: hostname(),
        platform: platform(),
        release: release(),
        machine_id: id.machineId,
      },
    }),
  });
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}: ${(await res.text().catch(() => "")).slice(0, 200)}`);
  }
  return unwrapData<SelfRegisterResponse>(await res.json());
}

async function fetchDeviceMe(apiUrl: string, secret: string): Promise<DeviceMeResponse> {
  const res = await fetch(new URL("/v1/devices/me", apiUrl), {
    headers: { authorization: `Bearer ${secret}` },
  });
  if (!res.ok) throw new Error(`devices/me ${res.status}`);
  return unwrapData<DeviceMeResponse>(await res.json());
}
