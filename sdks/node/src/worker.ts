/**
 * The background worker: the only place redaction, batching, and network I/O
 * happen. It owns the bounded buffer, drains it on a timer or when a batch
 * fills, converts captured calls into a wire batch, and ships it via the
 * {@link Transport}.
 *
 * The Rust reference is a single Tokio task driven by `select!`. Node has no
 * such runtime, so the equivalent here is:
 *   - a bounded array buffer the hot path pushes into (dropping the newest on
 *     overflow, never blocking);
 *   - an unref'd `setInterval` that triggers periodic flushes;
 *   - a serialized async flush (only one in flight at a time) so batches never
 *     overlap or interleave their sequence counters.
 */

import { buildBatch, type LlmCall, type SeqRef } from "./capture.js";
import type { Config } from "./config.js";
import { ambientMetadata } from "./context.js";
import type { Transport } from "./transport.js";

/** Sleep for `ms` milliseconds. */
function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Owns the buffer, the flush timer, and the transport. Cheap to hold a single
 * instance per process; the {@link Client} delegates its hot path here.
 */
export class Worker {
  private readonly cfg: Config;
  private readonly transport: Transport;

  /** Bounded buffer between the hot path and the flush loop. */
  private buffer: LlmCall[] = [];
  /** Monotonic per-call sequence counter (keeps dedupe keys distinct). */
  private readonly seq: SeqRef = { value: 0 };
  /** Count of calls dropped due to buffer overflow (a backpressure signal). */
  private droppedCount = 0;

  /** The periodic flush timer; `undefined` once shut down. */
  private timer: ReturnType<typeof setInterval> | undefined;
  /** Set once `shutdown()` has run, so late records are ignored. */
  private closed = false;
  /** Guard so the "local daemon unreachable" hint is printed at most once,
   * not on every dropped batch. */
  private warnedLocalDaemon = false;

  /**
   * Serializes flushes: every flush chains onto this promise so only one runs
   * at a time and `flush()`/`shutdown()` can await the tail.
   */
  private flushChain: Promise<void> = Promise.resolve();

  constructor(cfg: Config, transport: Transport) {
    this.cfg = cfg;
    this.transport = transport;

    // Periodic flush. `unref()` so a pending timer never keeps the Node process
    // alive on its own (mirrors "an idle SDK is silent / non-blocking").
    this.timer = setInterval(() => {
      void this.scheduleFlush();
    }, cfg.flushIntervalMs);
    this.timer.unref?.();
  }

  /**
   * Hot path: a non-blocking push into the bounded buffer. If the buffer is
   * full the newest call is dropped and the dropped-counter increments — the
   * caller is never blocked and never does I/O or redaction here.
   */
  record(call: LlmCall): void {
    if (this.closed) {
      return;
    }
    if (this.buffer.length >= this.cfg.bufferCapacity) {
      // Buffer full: drop the newest (the call we were just handed).
      this.droppedCount += 1;
      return;
    }
    // Snapshot the ambient metadata layer here, on the hot path, while any
    // `withMetadata(...)` scope is still active — the actual merge runs later on
    // the flush path, outside that scope. Only set when the caller didn't
    // already pin a snapshot.
    if (call.ambientMetadataSnapshot === undefined) {
      const ambient = ambientMetadata();
      if (ambient !== undefined) {
        call.ambientMetadataSnapshot = ambient;
      }
    }
    this.buffer.push(call);
    // Flush eagerly once a full batch has accumulated.
    if (this.buffer.length >= this.cfg.flushMaxBatch) {
      void this.scheduleFlush();
    }
  }

  /** Number of calls dropped due to buffer overflow. */
  dropped(): number {
    return this.droppedCount;
  }

  /** Flush buffered calls and wait for the worker to ship them. */
  async flush(): Promise<void> {
    await this.scheduleFlush();
  }

  /** Flush on the way out, then stop the timer. */
  async shutdown(): Promise<void> {
    this.closed = true;
    if (this.timer !== undefined) {
      clearInterval(this.timer);
      this.timer = undefined;
    }
    await this.scheduleFlush();
  }

  /**
   * Append a flush onto the serialized chain and return a promise that resolves
   * when *that* flush (and everything queued before it) has finished. Errors in
   * one flush never poison the chain.
   */
  private scheduleFlush(): Promise<void> {
    const next = this.flushChain.then(() => this.flushOnce());
    // Swallow rejections on the stored chain so a failed flush doesn't break
    // subsequent ones; callers awaiting `next` still observe completion.
    this.flushChain = next.catch(() => {});
    return next;
  }

  /**
   * Convert and ship the currently-buffered calls. Retries once after 250ms on
   * failure, then drops the batch loudly (in `local_daemon` mode the daemon
   * owns durable retry; remote durability is a follow-up).
   */
  private async flushOnce(): Promise<void> {
    if (this.buffer.length === 0) {
      return;
    }
    // Drain the buffer atomically (synchronous swap — no await in between).
    const drained = this.buffer;
    this.buffer = [];

    const batch = buildBatch(this.cfg, drained, this.seq);

    // Two attempts total: the initial send, then one retry after 250ms.
    for (let attempt = 0; attempt < 2; attempt++) {
      try {
        await this.transport.send(batch);
        return;
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        if (attempt === 0) {
          console.error(`modelstat: send failed (retrying once): ${msg}`);
          await sleep(250);
        } else {
          console.error(
            `modelstat: dropping batch of ${batch.events.length} events after retry: ${msg}`,
          );
          // The most common cause in the default `local_daemon` mode is that
          // no daemon is listening on loopback. Point the user at the fix
          // once (not per dropped batch) so a misconfigured setup is obvious
          // instead of silently losing data.
          if (this.cfg.mode.kind === "local_daemon" && !this.warnedLocalDaemon) {
            this.warnedLocalDaemon = true;
            console.error(
              `modelstat: the local daemon at ${this.cfg.mode.url} is unreachable — ` +
                "is it running? Install it with `curl -fsSL https://modelstat.ai/install.sh | sh`, " +
                "or ship directly to the server with `cfg.withRemote(baseUrl, raw)`. " +
                "(This hint prints once.)",
            );
          }
        }
      }
    }
  }
}
