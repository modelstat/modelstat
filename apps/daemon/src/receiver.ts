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
      await uploadBatch(shipped);
      await q.markSent(
        batch.events.map((e) => e.source_event_id),
        batch.batch_id,
      );
      events += batch.events.length;
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

/**
 * Start the loopback receiver. Best-effort: a busy port (a stale listener;
 * the singleton daemon lock already prevents a second daemon) disables the
 * SDK path with a warning rather than crashing the daemon's core file-scan
 * duty. Resolves `null` when the receiver could not bind.
 */
export function startLocalIngestReceiver(
  opts: { port?: number } = {},
): Promise<LocalIngestReceiver | null> {
  const port =
    opts.port ?? (Number(process.env.MODELSTAT_LOCAL_INGEST_PORT) || DEFAULT_LOCAL_INGEST_PORT);
  return new Promise((resolve) => {
    const server = createServer((req, res) => void handle(req, res));
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

async function handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const send = (code: number, body: unknown): void => {
    res.writeHead(code, { "content-type": "application/json" });
    res.end(JSON.stringify(body));
  };
  const path = (req.url ?? "").split("?")[0];
  if (req.method === "GET" && path === "/healthz") return send(200, { ok: true });
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
