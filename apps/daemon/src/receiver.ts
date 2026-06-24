/**
 * Local loopback ingest receiver — the server half of the SDKs'
 * `local_daemon` transport mode (sdks/{node,python,rust}; their
 * `DEFAULT_DAEMON_URL` is `http://127.0.0.1:4319/v1/ingest`).
 *
 * An SDK in `local_daemon` mode POSTs an `IngestBatch` of its own LLM-call
 * captures to loopback. The daemon owns the rest of the contract:
 *
 *   1. durable retry — events land in a {@link FileQueueStore} and survive a
 *      restart / offline window (the SDK is fire-and-forget; see the SDK's
 *      transport.ts: "the local daemon owns durable retry");
 *   2. on-device summarisation — the drain runs the SAME pipeline the file
 *      scan uses (redact → segment → summarise → tag), so only the daemon's
 *      redacted segment abstracts leave the machine (the raw per-turn excerpt
 *      is stripped before upload — see {@link drainLocalQueue});
 *   3. authenticated upload — under THIS device's secret, so the SDK ships
 *      zero credentials.
 *
 * Trust is loopback: the server binds `127.0.0.1` only and accepts any local
 * POST (same-user threat model). No request signing, no token — the daemon,
 * not the SDK, holds the device identity.
 */
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { homedir } from "node:os";
import { resolve, sep } from "node:path";
import type { RawEvent } from "@modelstat/core";
import { createLogger } from "@modelstat/daemon-core/logger";
import { FileQueueStore } from "@modelstat/daemon-core/node";
import {
  buildBatches,
  type PipelineRunner,
  type ToolCallDraft,
} from "@modelstat/daemon-core/queue";
import { uploadBatch } from "./api.js";
import { homePath } from "./paths.js";
import { buildSegments } from "./pipeline.js";

const logger = createLogger("daemon.local-ingest");

/** Default loopback port — must match the SDKs' `DEFAULT_DAEMON_URL`. */
export const DEFAULT_LOCAL_INGEST_PORT = 4319;
/** Cap on one POST body. SDK batches are small (≤256 events per flush);
 * this is a generous abuse guard, not a tuning knob. */
const MAX_BODY_BYTES = 16 * 1024 * 1024;

/** What `POST /v1/control/scan` asks the daemon to force-scan. `file` is one
 * explicit transcript path; `session_ids` a compaction chain; neither = the
 * newest transcript. `wait` makes the response block until the scan + insight
 * refresh finish (used by `modelstat sync --session --wait`). */
export interface ControlScanRequest {
  session_ids?: string[];
  file?: string;
  wait?: boolean;
}

/** The daemon-supplied force-scan worker, injected so receiver.ts has no
 * import cycle back to scan.ts / daemon.ts. Resolves when the scan (and the
 * subsequent server insight refresh) have completed. */
export type ControlScanHandler = (target: {
  sessionIds?: string[];
  file?: string;
}) => Promise<void>;

/** Roots an explicit `file` target is allowed to live under — the same
 * transcript directories the scanner discovers. Anything else is rejected so a
 * local POST can't make the daemon parse (and ship summaries of) an arbitrary
 * file on disk. */
function transcriptRoots(): string[] {
  const home = homedir();
  return [resolve(home, ".claude/projects"), resolve(home, ".codex/sessions")];
}

/** True when `file` resolves to a path strictly inside one of the transcript
 * roots (defeats `..` traversal — we compare resolved, separator-bounded
 * prefixes). */
export function isAllowedTranscriptFile(file: string): boolean {
  const target = resolve(file);
  return transcriptRoots().some(
    (root) => target === root || target.startsWith(root + sep),
  );
}

let store: FileQueueStore | null = null;
function queue(): FileQueueStore {
  // A snapshot file distinct from the file-scan path's store, so
  // SDK-sourced and file-scanned events never share an on-disk document.
  if (!store) store = new FileQueueStore(homePath("sdk-ingest-queue.json"));
  return store;
}

const pipeline: PipelineRunner = { run: (events) => buildSegments(events) };

interface WireBatch {
  events: RawEvent[];
  // The SDK wire carries ToolCallWire (which has `segment_id`); we strip it
  // and re-attribute from the daemon's own segments at batch-build.
  toolCalls: Array<ToolCallDraft & { source_event_id: string; segment_id?: string | null }>;
}

/** Minimal structural validation. The authoritative validator is the backend
 * (Rust serde — which the uploader's `400 = drop` retry matrix already
 * handles, and which is leniently optional on the nullable fields the SDK
 * omits, unlike the strict `@modelstat/core` zod schema). Here we check only
 * the fields needed to build {@link QueueItem}s and pass the rest through. */
function parseBatch(json: unknown): WireBatch | { error: string } {
  if (typeof json !== "object" || json === null) return { error: "body must be a JSON object" };
  const b = json as Record<string, unknown>;
  if (!Array.isArray(b.events) || b.events.length === 0)
    return { error: "events must be a non-empty array" };
  if (b.events.length > 10_000) return { error: "too many events (max 10000)" };
  for (const e of b.events) {
    if (typeof e !== "object" || e === null) return { error: "each event must be an object" };
    const ev = e as Record<string, unknown>;
    for (const k of ["source_event_id", "session_id", "agent", "ts"] as const) {
      if (typeof ev[k] !== "string" || (ev[k] as string).length === 0)
        return { error: `event.${k} is required` };
    }
  }
  const toolCalls = Array.isArray(b.tool_calls) ? (b.tool_calls as WireBatch["toolCalls"]) : [];
  return { events: b.events as RawEvent[], toolCalls };
}

/** Enqueue one batch's events. Idempotent: {@link FileQueueStore} dedupes by
 * `source_event_id`, so a retried POST (the SDK retries once) is a no-op. */
async function enqueue(batch: WireBatch): Promise<number> {
  const q = queue();
  for (const event of batch.events) {
    const calls: ToolCallDraft[] = batch.toolCalls
      .filter((tc) => tc.source_event_id === event.source_event_id)
      // Drop any `segment_id` the SDK sent — it's (re)attributed at
      // batch-build from the daemon's own segments. `put` enqueues a draft.
      .map(({ segment_id: _segmentId, ...draft }) => draft as ToolCallDraft);
    await q.put({
      source_event_id: event.source_event_id,
      session_id: event.session_id,
      agent: event.agent,
      event,
      last_event_ts_ms: Date.parse(event.ts) || Date.now(),
      synced: false,
      sent_batch_id: null,
      tool_calls: calls.length > 0 ? calls : undefined,
    });
  }
  return batch.events.length;
}

let draining = false;
/**
 * One drain pass: build batches from the durable queue (per-session
 * debounce + on-device summariser, via {@link buildBatches}), strip the raw
 * per-turn excerpt so only redacted segment abstracts leave the machine,
 * upload under the device secret, then mark the shipped events sent.
 *
 * Coalesced — never overlaps itself; the bundled summariser also serialises
 * its own calls, so this runs safely alongside the file-scan path. On upload
 * failure the events stay durably queued and the next tick retries.
 */
export async function drainLocalQueue(opts: {
  deviceId: string;
  daemonVersion: string;
}): Promise<{ batches: number; events: number }> {
  if (draining) return { batches: 0, events: 0 };
  draining = true;
  try {
    const q = queue();
    if ((await q.countUnsent()) === 0) return { batches: 0, events: 0 };
    const batches = await buildBatches({
      store: q,
      pipeline,
      deviceId: opts.deviceId,
      daemonVersion: opts.daemonVersion,
      nowMs: Date.now(),
    });
    let events = 0;
    for (const batch of batches) {
      // Privacy: the daemon has already produced redacted segment abstracts;
      // the per-event turn excerpt is only summariser input and must not
      // leave the machine — honours the SDK's "raw text never leaves the
      // machine" contract even when the SDK shipped with redaction "none".
      const shipped = {
        ...batch,
        events: batch.events.map(({ content_excerpt: _excerpt, ...rest }) => rest),
      };
      const res = await uploadBatch(shipped);
      if (!res.committed) {
        // PERMANENT reject (400/422). Mark sent anyway so this one poison batch
        // doesn't wedge the queue — a TRANSIENT failure throws above and is
        // retried, never marked. Loud: it's daemon-side data loss.
        console.error(
          `SDK ingest batch ${batch.batch_id} dropped — server rejected it (${res.reason}); skipping so the queue keeps draining`,
        );
      }
      await q.markSent(
        batch.events.map((e) => e.source_event_id),
        batch.batch_id,
      );
      if (res.committed) events += batch.events.length;
    }
    return { batches: batches.length, events };
  } finally {
    draining = false;
  }
}

/** Unsent depth — surfaced in the heartbeat so the dashboard shows SDK
 * backlog draining. */
export function localQueueDepth(): Promise<number> {
  return queue().countUnsent();
}

export interface LocalIngestReceiver {
  readonly port: number;
  close(): Promise<void>;
}

export interface StartReceiverOptions {
  port?: number;
  /** Optional force-scan worker. When provided, the receiver serves
   * `POST /v1/control/scan` (loopback) so `modelstat sync --session` can warm
   * a running daemon instead of cold-loading its own summariser. Omitted (e.g.
   * in tests) → that route 404s. */
  onControlScan?: ControlScanHandler;
}

/**
 * Start the loopback receiver. Best-effort: a busy port (a stale listener;
 * the singleton daemon lock already prevents a second daemon) disables the
 * SDK path with a warning rather than crashing the daemon's core file-scan
 * duty. Resolves `null` when the receiver could not bind.
 */
export function startLocalIngestReceiver(
  opts: StartReceiverOptions = {},
): Promise<LocalIngestReceiver | null> {
  const port =
    opts.port ?? (Number(process.env.MODELSTAT_LOCAL_INGEST_PORT) || DEFAULT_LOCAL_INGEST_PORT);
  // Single-flight the control scan: a burst of /v1/control/scan posts (the
  // plugin + statusline can both fire on one turn) coalesces into at most one
  // in-flight scan plus one queued follow-up, mirroring the daemon's own
  // scan-coalescing so two eager scans never run concurrently.
  const controlRunner = opts.onControlScan
    ? createControlRunner(opts.onControlScan)
    : null;
  return new Promise((resolve) => {
    const server = createServer((req, res) => void handle(req, res, controlRunner));
    let settled = false;
    server.on("error", (err: NodeJS.ErrnoException) => {
      if (settled) return;
      settled = true;
      logger.warn(
        `local ingest receiver disabled — SDK local_daemon mode unavailable: ${err.code ?? err.message} on 127.0.0.1:${port}`,
      );
      resolve(null);
    });
    server.listen(port, "127.0.0.1", () => {
      settled = true;
      // With port 0 the OS assigns a free port (used by tests); report the
      // actual bound port, not the requested one.
      const addr = server.address();
      const boundPort = typeof addr === "object" && addr ? addr.port : port;
      logger.info(`local ingest receiver on http://127.0.0.1:${boundPort}/v1/ingest`);
      resolve({
        port: boundPort,
        close: () => new Promise<void>((r) => server.close(() => r())),
      });
    });
  });
}

/** Coalescing wrapper around the force-scan handler. Returns the promise for
 * the run that will cover this request's target — a `wait:true` caller awaits
 * it; a fire-and-forget caller ignores it. Errors are surfaced to the awaiter
 * but never crash the receiver. */
interface ControlRunner {
  run(target: { sessionIds?: string[]; file?: string }): Promise<void>;
}
function createControlRunner(handler: ControlScanHandler): ControlRunner {
  let active: Promise<void> | null = null;
  return {
    run(target) {
      // Chain onto any in-flight scan so we never run two at once; the new
      // target runs after the current one drains.
      const prev = active ?? Promise.resolve();
      const next = prev.catch(() => undefined).then(() => handler(target));
      active = next.catch(() => undefined);
      return next;
    },
  };
}

async function handle(
  req: IncomingMessage,
  res: ServerResponse,
  controlRunner: ControlRunner | null,
): Promise<void> {
  const send = (code: number, body: unknown): void => {
    res.writeHead(code, { "content-type": "application/json" });
    res.end(JSON.stringify(body));
  };
  const path = (req.url ?? "").split("?")[0];
  if (req.method === "GET" && path === "/healthz") return send(200, { ok: true });
  if (req.method === "POST" && path === "/v1/control/scan") {
    return handleControlScan(req, send, controlRunner);
  }
  // SDK local_daemon mode posts to /v1/ingest; accept /raw too (the daemon
  // redacts + summarises regardless of which door the SDK used).
  if (req.method !== "POST" || (path !== "/v1/ingest" && path !== "/v1/ingest/raw")) {
    return send(404, { error: "not found" });
  }
  let size = 0;
  const chunks: Buffer[] = [];
  try {
    for await (const chunk of req) {
      size += (chunk as Buffer).length;
      if (size > MAX_BODY_BYTES) {
        send(413, { error: "batch too large" });
        req.destroy();
        return;
      }
      chunks.push(chunk as Buffer);
    }
  } catch {
    return send(400, { error: "read error" });
  }
  let json: unknown;
  try {
    json = JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    return send(400, { error: "invalid json" });
  }
  const parsed = parseBatch(json);
  if ("error" in parsed) return send(400, parsed);
  try {
    const accepted = await enqueue(parsed);
    return send(200, { accepted, queued: true });
  } catch (e) {
    logger.warn(`local ingest enqueue failed: ${(e as Error).message}`);
    return send(500, { error: "enqueue failed" });
  }
}

/** Read + validate a control-scan body, then dispatch the force-scan. With
 * `wait:true` the response blocks until the scan + insight refresh finish;
 * otherwise it returns immediately with `{started:true}`. */
async function handleControlScan(
  req: IncomingMessage,
  send: (code: number, body: unknown) => void,
  controlRunner: ControlRunner | null,
): Promise<void> {
  if (!controlRunner) return send(503, { error: "control scan unavailable" });
  // Bodies are tiny (a few ids); a generous small cap is plenty.
  const chunks: Buffer[] = [];
  let size = 0;
  try {
    for await (const chunk of req) {
      size += (chunk as Buffer).length;
      if (size > 64 * 1024) {
        send(413, { error: "control body too large" });
        req.destroy();
        return;
      }
      chunks.push(chunk as Buffer);
    }
  } catch {
    return send(400, { error: "read error" });
  }
  let body: ControlScanRequest;
  try {
    const raw = chunks.length ? Buffer.concat(chunks).toString("utf8") : "{}";
    body = JSON.parse(raw) as ControlScanRequest;
  } catch {
    return send(400, { error: "invalid json" });
  }
  const sessionIds = Array.isArray(body.session_ids)
    ? body.session_ids.filter((s): s is string => typeof s === "string" && s.length > 0)
    : undefined;
  let file: string | undefined;
  if (body.file !== undefined) {
    if (typeof body.file !== "string" || !isAllowedTranscriptFile(body.file)) {
      return send(400, { error: "file must be under ~/.claude/projects or ~/.codex/sessions" });
    }
    file = body.file;
  }
  const target = { sessionIds, file };
  const run = controlRunner.run(target);
  if (body.wait === true) {
    try {
      await run;
      return send(200, { ok: true, scanned: true });
    } catch (e) {
      return send(500, { error: `scan failed: ${(e as Error).message}` });
    }
  }
  // Fire-and-forget: don't let an unawaited rejection crash the process.
  void run.catch((e) => logger.warn(`control scan failed: ${(e as Error).message}`));
  return send(200, { ok: true, started: true });
}
