/**
 * SDK configuration: where to ship, how to authenticate, how hard to redact,
 * and how the background worker batches.
 */

import { CLIENT_VERSION } from "./version.js";
import type { Metadata } from "./wire.js";

/** The default local daemon loopback ingest URL. */
export const DEFAULT_DAEMON_URL = "http://127.0.0.1:4319/v1/ingest";

/**
 * Where the SDK ships captured calls.
 *
 * - `local_daemon` (default): hand off to a local modelstat daemon over
 *   loopback. The daemon summarizes with its local Qwen model and ships only
 *   redacted abstracts to the server. Raw text never leaves the machine.
 * - `remote`: ship directly to the modelstat server (no local daemon / no
 *   local model). With `raw = true`, send full (still floor-redacted) turns to
 *   `/v1/ingest/raw` for server-side summarization; with `raw = false`, send
 *   only the floor-redacted ≤320-char excerpt to `/v1/ingest`.
 */
export type Mode =
  | { kind: "local_daemon"; url: string }
  | { kind: "remote"; baseUrl: string; raw: boolean };

/** Resolve the concrete POST endpoint for a mode. */
export function endpoint(mode: Mode): string {
  if (mode.kind === "local_daemon") {
    return mode.url;
  }
  // Trim any trailing slashes from the base url before appending the path.
  const base = mode.baseUrl.replace(/\/+$/, "");
  return mode.raw ? `${base}/v1/ingest/raw` : `${base}/v1/ingest`;
}

/**
 * How hard to scrub text before it leaves the SDK process.
 *
 * - `"floor"` (default): run the privacy floor (secrets + email + absolute
 *   paths). The floor that even "raw" mode keeps.
 * - `"none"`: skip in-process redaction entirely. Only valid when shipping to a
 *   trusted local daemon that will redact, or under an explicit raw-data
 *   contract.
 */
export type RedactionPolicy = "floor" | "none";

/**
 * SDK configuration. Construct with `new Config(ingestKey, agent)` then adjust
 * fields, or chain the `with*` setters.
 */
export class Config {
  /**
   * The integration's own name — which app/service this SDK instance
   * instruments (e.g. `checkout-api`, `billing-worker`). **Required**, and the
   * server rejects a batch that does not carry it (`app_required`).
   *
   * This is the name your usage appears under in the dashboard: the server
   * registers a real device per (org, app) and a real account per
   * (provider, app) from it, so SDK traffic is attributed at ingest like any
   * other. There is deliberately no default — a name guessed from the process
   * would silently merge two services that share a binary, and you would never
   * find out.
   */
  app: string;

  /**
   * Stable device/service identifier (`dev_…`). Leave the default: on an org
   * ingest key the server derives the real device identity from
   * (org, {@link app}) and ignores this. It matters only for advanced setups
   * shipping under a pre-registered device secret.
   */
  deviceId = "dev_sdk";

  /**
   * The **agent** label for every record — which AI tool/integration the user
   * used (e.g. `raw_sdk_openai`, `raw_sdk_anthropic`, `raw_sdk_generic`). Ships
   * as the wire `agent` field.
   */
  agent: string;

  /**
   * This client build's version (≤40 chars). Ships as the wire
   * `daemon_version` field — the *producer's* version (daemon or SDK), not the
   * agent's. (`node-sdk/<pkg version>`.)
   */
  version = CLIENT_VERSION;

  /** Bearer credential: an org-scoped ingest key (`msk_…`) or a device secret. */
  ingestKey: string;

  /** Where to ship. Defaults to the local daemon over loopback. */
  mode: Mode = { kind: "local_daemon", url: DEFAULT_DAEMON_URL };

  /** In-process redaction policy. Defaults to the privacy floor. */
  redaction: RedactionPolicy = "floor";

  /**
   * Bounded in-memory buffer between the hot path and the worker. On overflow
   * the newest record is dropped and the dropped-counter increments — the live
   * request is never blocked.
   */
  bufferCapacity = 4096;

  /** Flush the buffer at least this often (milliseconds). */
  flushIntervalMs = 2000;

  /** Flush eagerly once this many records are buffered. */
  flushMaxBatch = 256;

  /**
   * Whether the server should run taxonomy auto-detection on batches from this
   * client. Ships as the wire `auto_taxonomy` field. Defaults to `false` for
   * SDK/backend integrations — backend LLM usage isn't interactive
   * work-sessions, so taxonomy is **off by default**; set it to `true` to opt
   * in.
   */
  autoTaxonomy = false;

  /**
   * Constant attribution tags applied to **every** call (e.g.
   * `{ environment: "prod", service: "checkout" }`). The lowest-priority layer:
   * the ambient context layer ({@link withMetadata}) and per-call tags both win
   * on a shared key. Capped before send (≤16 entries; keys ≤64 chars; values
   * ≤256 chars). Empty by default.
   */
  metadata: Metadata = {};

  /**
   * A config with sane defaults: local-daemon mode, floor redaction, a 4096-
   * slot buffer, a 2s flush interval, and 256-record batches.
   *
   * @param ingestKey Bearer credential (`msk_…` org key or a device secret).
   * @param agent The AI-tool label shipped as the wire `agent` field.
   * @param app This service's own name — see {@link app}. Required.
   * @throws If `app` is empty or blank. Failing here, at startup, is the whole
   * point: the alternative is a process that ships happily for a week and whose
   * usage turns out to be filed under a shared placeholder nobody can claim.
   */
  constructor(ingestKey: string, agent: string, app: string) {
    if (app === undefined || app === null || app.trim() === "") {
      throw new Error(
        "modelstat: Config requires a non-empty `app` — the name of the service " +
          'you are instrumenting (e.g. new Config(key, agent, "checkout-api")). ' +
          "It becomes this integration's device and account name in the dashboard.",
      );
    }
    this.ingestKey = ingestKey;
    this.agent = agent;
    this.app = app.trim();
  }

  /**
   * Ship directly to the modelstat server instead of a local daemon.
   * `raw = true` opts into server-side summarization of full (floor-redacted)
   * turns. Returns `this` for chaining.
   */
  withRemote(baseUrl: string, raw: boolean): this {
    this.mode = { kind: "remote", baseUrl, raw };
    return this;
  }

  /** Override the device id. Returns `this` for chaining. */
  withDeviceId(deviceId: string): this {
    this.deviceId = deviceId;
    return this;
  }

  /**
   * Whether this mode sends full (untruncated) redacted turns for server-side
   * summarization — i.e. remote mode with `raw = true`.
   */
  sendsFullTurns(): boolean {
    return this.mode.kind === "remote" && this.mode.raw;
  }
}
