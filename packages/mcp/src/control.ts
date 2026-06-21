/**
 * Kick a running local daemon to force-scan the current session, via the
 * loopback control endpoint the daemon serves (apps/daemon/src/receiver.ts:
 * `POST 127.0.0.1:<port>/v1/control/scan`). Used by the `session_insights`
 * eager flow: scan the just-finished session locally FIRST, then forward the
 * tool call to the server so it returns fresh numbers.
 *
 * If no daemon is listening (ECONNREFUSED) we proceed anyway — the server
 * still answers (likely `not_ingested`/`analyzing`), and the SDK / user can
 * re-poll. Best-effort throughout: a control-kick failure never blocks the
 * tool call.
 */
const DEFAULT_CONTROL_PORT = 4319;

function controlPort(): number {
  return Number(process.env.MODELSTAT_LOCAL_INGEST_PORT) || DEFAULT_CONTROL_PORT;
}

export type ControlKickResult =
  | { kind: "scanned" }
  | { kind: "no_daemon" }
  | { kind: "error"; message: string };

/**
 * Ask the local daemon to force-scan `sessionIds` and wait for it to finish
 * (so the subsequent server call sees the freshly-ingested session). Bounded
 * by `timeoutMs` — a slow/cold daemon shouldn't hang the tool call; on timeout
 * we just return and let the server answer with whatever it has.
 */
export async function kickDaemonScan(
  sessionIds: string[],
  opts: { wait?: boolean; timeoutMs?: number } = {},
): Promise<ControlKickResult> {
  if (sessionIds.length === 0) return { kind: "no_daemon" };
  const wait = opts.wait !== false;
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), opts.timeoutMs ?? 15_000);
  try {
    const res = await fetch(`http://127.0.0.1:${controlPort()}/v1/control/scan`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ session_ids: sessionIds, wait }),
      signal: controller.signal,
    });
    if (!res.ok) {
      return { kind: "error", message: `control scan ${res.status}` };
    }
    await res.json().catch(() => ({}));
    return { kind: "scanned" };
  } catch (e) {
    const code = (e as { cause?: { code?: string } }).cause?.code;
    if (code === "ECONNREFUSED" || code === "ECONNRESET") return { kind: "no_daemon" };
    // AbortError (timeout) or any other failure → treat as "couldn't, proceed".
    return { kind: "error", message: (e as Error).message };
  } finally {
    clearTimeout(timeout);
  }
}
