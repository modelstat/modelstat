/**
 * How a built batch leaves the worker. The {@link Transport} interface lets
 * tests run the whole pipeline in-process (via {@link FakeTransport}) and lets
 * the daemon / server paths share one worker.
 */

import { endpoint, type Config } from "./config.js";
import type { IngestBatch } from "./wire.js";

/**
 * A transport error. The worker retries once, then drops the batch (the local
 * daemon, in `local_daemon` mode, owns durable retry).
 */
export class TransportError extends Error {
  /** Set when the failure was a non-2xx HTTP status. */
  readonly status?: number;

  constructor(message: string, status?: number) {
    super(message);
    this.name = "TransportError";
    this.status = status;
  }

  /** A non-2xx HTTP status failure. */
  static status(code: number): TransportError {
    return new TransportError(`http status ${code}`, code);
  }

  /** Any other (network / serialization) failure. */
  static other(message: string): TransportError {
    return new TransportError(`transport: ${message}`);
  }
}

/** Ships a built batch to its destination. */
export interface Transport {
  send(batch: IngestBatch): Promise<void>;
}

/** In-memory transport for tests: records every batch it is handed. */
export class FakeTransport implements Transport {
  private readonly recorded: IngestBatch[] = [];

  send(batch: IngestBatch): Promise<void> {
    // Store a structural clone so later mutation of the source can't change a
    // recorded batch (mirrors the Rust `FakeTransport` cloning the batch).
    this.recorded.push(structuredClone(batch));
    return Promise.resolve();
  }

  /** Snapshot of every batch sent so far. */
  batches(): IngestBatch[] {
    return [...this.recorded];
  }
}

/** The real HTTP transport: `POST <endpoint>` with a bearer ingest key. */
export class HttpTransport implements Transport {
  private readonly endpoint: string;
  private readonly bearer: string;

  constructor(endpointUrl: string, bearer: string) {
    this.endpoint = endpointUrl;
    this.bearer = bearer;
  }

  /** Build a transport from a {@link Config}, resolving its mode's endpoint. */
  static fromConfig(cfg: Config): HttpTransport {
    return new HttpTransport(endpoint(cfg.mode), cfg.ingestKey);
  }

  async send(batch: IngestBatch): Promise<void> {
    let resp: Response;
    try {
      resp = await fetch(this.endpoint, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${this.bearer}`,
        },
        body: JSON.stringify(batch),
      });
    } catch (e) {
      // Network-level failure (DNS, connection refused, etc.).
      throw TransportError.other(e instanceof Error ? e.message : String(e));
    }
    if (!resp.ok) {
      throw TransportError.status(resp.status);
    }
  }
}
