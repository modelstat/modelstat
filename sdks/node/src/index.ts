/**
 * # @modelstat/sdk
 *
 * A privacy-first SDK for wrapping the LLM calls your backend already makes and
 * shipping **redacted** usage to modelstat — without adding latency to live
 * requests.
 *
 * The hot path ({@link Client.record}) does nothing but push your
 * already-in-hand call into a bounded buffer and return. A background worker
 * redacts, batches, and ships off the request path. On overflow the newest
 * record is dropped and a counter increments — your request is never blocked
 * and never grows memory unbounded.
 *
 * ## Modes
 *
 * - **Local daemon (default).** Hand calls to a local modelstat daemon over
 *   loopback; it summarizes with a local Qwen model and ships only redacted
 *   abstracts. Raw text never leaves the machine.
 * - **Remote.** Ship directly to the modelstat server (no local model). With
 *   `raw = true`, send full floor-redacted turns for server-side
 *   summarization.
 *
 * ```ts
 * import { Client, Config, LlmCall } from "@modelstat/sdk";
 *
 * // Org-scoped ingest key binds traffic to your account; remote mode here.
 * const cfg = new Config("msk_live_…", "raw_sdk_openai")
 *   .withRemote("https://api.modelstat.ai", true);
 * const ms = new Client(cfg);
 *
 * // ... after your real LLM call returns ...
 * ms.record(
 *   new LlmCall("openai", "session-or-trace-id")
 *     .model("gpt-x")
 *     .tokens({ input: 800, output: 120 })
 *     .text("the prompt", "the completion"),
 * );
 *
 * await ms.shutdown(); // flush on the way out
 * ```
 */

import { LlmCall } from "./capture.js";
import { Config } from "./config.js";
import { HttpTransport, type Transport } from "./transport.js";
import { Worker } from "./worker.js";

// Public surface (mirrors the Rust crate's `pub use`s).
export { LlmCall, type ToolCallInput } from "./capture.js";
export {
  Config,
  DEFAULT_DAEMON_URL,
  endpoint,
  type Mode,
  type RedactionPolicy,
} from "./config.js";
export { redact, type Redacted } from "./redact.js";
export {
  FakeTransport,
  HttpTransport,
  TransportError,
  type Transport,
} from "./transport.js";
export {
  batchId,
  contentHash,
  sourceEventId,
  totalTokens,
  zeroTokens,
  type PricingMode,
  type EventKind,
  type GitContext,
  type IngestBatch,
  type RawEvent,
  type TokenUsage,
  type ToolCallStatus,
  type ToolCallWire,
} from "./wire.js";

/**
 * The SDK handle. Construct one with `new Client(cfg)` and hand it to every
 * request handler; it owns a single shared buffer + background worker.
 */
export class Client {
  private readonly worker: Worker;

  /**
   * Start the SDK. With just a {@link Config}, the default HTTP transport for
   * `cfg.mode` is used; pass a `transport` to override it (or use the
   * {@link Client.withTransport} helper).
   */
  constructor(cfg: Config, transport?: Transport) {
    this.worker = new Worker(cfg, transport ?? HttpTransport.fromConfig(cfg));
  }

  /**
   * Start the SDK with a custom {@link Transport} (e.g. `FakeTransport` in
   * tests).
   */
  static withTransport(cfg: Config, transport: Transport): Client {
    return new Client(cfg, transport);
  }

  /**
   * Record a captured call. **Hot path:** a non-blocking push into the buffer.
   * If the buffer is full the call is dropped and {@link Client.dropped}
   * increments — the caller is never blocked.
   */
  record(call: LlmCall): void {
    this.worker.record(call);
  }

  /** Number of calls dropped due to buffer overflow (a backpressure signal). */
  dropped(): number {
    return this.worker.dropped();
  }

  /** Flush buffered calls and wait for the worker to ship them. */
  flush(): Promise<void> {
    return this.worker.flush();
  }

  /**
   * Flush on the way out and stop the background timer. The conventional
   * shutdown call.
   */
  shutdown(): Promise<void> {
    return this.worker.shutdown();
  }
}
