/**
 * Shared ingest HTTP client + retry matrix.
 *
 * Replaces:
 *   - CLI's `uploadBatch()` in apps/daemon/src/api.ts (throws on any non-2xx)
 *   - Extension's inline fetch in apps/extension/src/background/ingest-queue.ts
 *
 * Both daemons now call `IngestClient.upload(batch)` with identical
 * retry semantics, identical auth header shape, and an explicit
 * reauth hook that runs when the server rejects the token.
 */
import type { IngestBatch } from "@modelstat/core/schemas";
import { expBackoff } from "../config/index.js";
import type { Logger } from "../logger.js";
import { describeErrorWithCause } from "../logger.js";

export type UploadDecision =
  | { type: "commit" }
  | { type: "reauth" }
  | { type: "backoff"; delayMs: number };

/**
 * Classify an HTTP status into the next action to take. Shared across
 * daemons so "what happens on 401" is answered in exactly one place.
 */
export function classifyStatus(status: number, attempt: number): UploadDecision {
  if (status >= 200 && status < 300) return { type: "commit" };
  if (status === 401 || status === 403) return { type: "reauth" };
  // EVERYTHING else HOLDS + retries until it succeeds — the daemon never discards a
  // batch, so an outage or misconfig is a pause, never a gap in the data.
  //
  // We used to quarantine "permanent" 4xx (400/422) as un-acceptable payloads, but
  // in practice even those are usually transient or environmental — and the cost of
  // guessing wrong is unrecoverable data loss:
  //   · 400 / 403 / 429 can come from a Cloudflare/WAF edge, not our origin.
  //   · 404 / 405 — the route isn't reachable RIGHT NOW: undeployed, mis-proxied
  //     (observed live 2026-07-08: a Caddy exact-path matcher 405'd every cloud-mode
  //     POST /v1/ingest/raw), or renamed mid-rollout.
  //   · 413 / 415 / 422 usually clear once a server-side clamp/parse fix lands.
  //   · 408 / 426 / 429 — timeout; version-gate (daemon self-updates via heartbeat);
  //     rate-limit. 5xx — server down / restarting / bad gateway.
  // A held batch stalls only its own file's cursor — visible in the logs and it
  // drains once the server is healthy; a dropped batch is gone for good. So we hold.
  return { type: "backoff", delayMs: expBackoff(attempt) };
}

/** What a daemon provides so the ingest client can mint + refresh
 * bearer tokens. Token storage details stay in the daemon
 * (Conf file on Node, IndexedDB on browser). */
export interface AuthProvider {
  getToken(): Promise<string | null>;
  /** Called exactly when the server returns 401/403. The daemon
   * should attempt a reauth (e.g. self-register + claim rotation) and
   * return `true` if a fresh token is now available via `getToken()`. */
  onInvalidToken(): Promise<boolean>;
}

export interface IngestResponse {
  accepted: number;
  new_sessions: number;
  updated_sessions: number;
  batch_id: string;
  raw_s3_key: string | null;
}

export interface IngestClientOpts {
  apiUrl: string;
  auth: AuthProvider;
  logger: Logger;
  /** Override fetch for tests; defaults to global fetch. */
  fetchImpl?: typeof fetch;
  /** Upper bound on retry attempts for one upload call. Default 3. */
  maxAttempts?: number;
}

/**
 * JSON.stringify that never emits lone UTF-16 surrogates.
 *
 * Excerpts and abstracts are truncated with `.slice(0, N)` in several
 * places (parsers, pipeline); a slice that lands mid-emoji leaves a
 * lone leading surrogate at the end of the string. JS tolerates it and
 * JSON.stringify dutifully emits `"\ud83d"` — but the ingest server's
 * strict JSON decoder rejects the escape ("unexpected end of hex
 * escape", invalid_json, HTTP 400) and the WHOLE batch is dropped.
 * Because the same event re-parses on every scan, one sliced emoji
 * wedged ingestion permanently (observed live 2026-06-12). Mapping
 * every string through toWellFormed() (lone surrogate → U+FFFD) at the
 * wire boundary makes the payload parseable by any strict decoder.
 */
export function wellFormedStringify(value: unknown): string {
  return JSON.stringify(value, (_key, v) => (typeof v === "string" ? v.toWellFormed() : v));
}

export class IngestClient {
  private readonly fetchImpl: typeof fetch;
  private readonly maxAttempts: number;
  constructor(private readonly opts: IngestClientOpts) {
    this.fetchImpl = opts.fetchImpl ?? globalThis.fetch.bind(globalThis);
    this.maxAttempts = opts.maxAttempts ?? 3;
  }

  /**
   * Upload a batch. `opts.raw` targets `/v1/ingest/raw` — the endpoint for
   * cleaned FULL turns the server summarises itself (cloud mode), as opposed to
   * `/v1/ingest` for pre-summarised abstracts. Same auth, retry, and reauth
   * matrix either way.
   */
  async upload(batch: IngestBatch, opts: { raw?: boolean } = {}): Promise<UploadResult> {
    const path = opts.raw ? "/v1/ingest/raw" : "/v1/ingest";
    const url = `${this.opts.apiUrl.replace(/\/+$/, "")}${path}`;
    // Track the last network-level failure so we can surface its real
    // cause chain (ENOTFOUND, ECONNREFUSED, CERT_HAS_EXPIRED, etc)
    // instead of the useless "fetch failed" message undici wraps.
    let lastFetchError: string | null = null;
    for (let attempt = 0; attempt < this.maxAttempts; attempt++) {
      const token = await this.opts.auth.getToken();
      if (!token) {
        const reauthed = await this.opts.auth.onInvalidToken();
        if (!reauthed) return { kind: "drop", reason: "no_token", permanent: false };
        continue;
      }
      let res: Response;
      try {
        res = await this.fetchImpl(url, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            authorization: `Bearer ${token}`,
          },
          // wellFormedStringify, not JSON.stringify: a truncated-emoji
          // lone surrogate in any excerpt 400s the whole batch on the
          // ingest server's strict JSON decoder. See the helper's doc comment.
          body: wellFormedStringify(batch),
        });
      } catch (err) {
        const detail = describeErrorWithCause(err);
        lastFetchError = detail;
        this.opts.logger.warn("ingest fetch failed", {
          err: detail,
          url,
          attempt,
        });
        await sleep(expBackoff(attempt));
        continue;
      }
      const decision = classifyStatus(res.status, attempt);
      if (decision.type === "commit") {
        const body = (await res.json().catch(() => ({}))) as IngestResponse;
        return { kind: "commit", response: body };
      }
      if (decision.type === "reauth") {
        // Release the unread body before retrying — with global fetch
        // (undici) an unconsumed body keeps the connection + its
        // buffered bytes alive until GC, which leaks across a reauth
        // loop during a long backfill.
        await res.body?.cancel().catch(() => {});
        const ok = await this.opts.auth.onInvalidToken();
        if (!ok) return { kind: "drop", reason: "reauth_failed", permanent: false };
        continue; // retry with new token
      }
      // backoff + HOLD (we never drop). For a 4xx the body usually says WHY the
      // batch was rejected (a WAF/edge notice, a validation message) — surface it so
      // a batch that's genuinely stuck is diagnosable, not an opaque repeating
      // "backoff 400". For 5xx/429 the body is noise, and a backfill looping here on
      // sustained 429s would pile un-cancelled bodies on the heap, so drain it unread
      // (see reauth note).
      let body: string | undefined;
      if (res.status >= 400 && res.status < 500) {
        body = (await res.text().catch(() => "")).slice(0, 500) || undefined;
      } else {
        await res.body?.cancel().catch(() => {});
      }
      this.opts.logger.warn("ingest backoff", {
        status: res.status,
        delay_ms: decision.delayMs,
        attempt,
        ...(body ? { body } : {}),
      });
      await sleep(decision.delayMs);
    }
    // Out of attempts. If every attempt failed at the fetch layer,
    // surface the underlying cause in the drop reason so the daemon's
    // "Upload failed: X" message becomes actionable
    // (ECONNREFUSED/ENOTFOUND/etc) instead of "fetch failed".
    const reason = lastFetchError ? `network: ${lastFetchError}` : "max_attempts_exceeded";
    // Transient: the batch is fine, the server/network wasn't — HOLD + retry.
    return { kind: "drop", reason, permanent: false };
  }
}

export type UploadResult =
  | { kind: "commit"; response: IngestResponse }
  // A non-commit is always a HOLD: the batch wasn't accepted (no token / reauth
  // failed / retries exhausted on any 4xx/5xx / network), so the caller retries it
  // next cycle — good data is NEVER discarded. `permanent` is retained for wire
  // compatibility but is now ALWAYS `false`: classifyStatus no longer marks any
  // response as un-retryable (see its comment), so the scan loop's quarantine branch
  // is dormant. Deleting that path (and this field) end-to-end is a follow-up.
  | { kind: "drop"; reason: string; permanent: boolean };

function sleep(ms: number): Promise<void> {
  return new Promise((r) => {
    setTimeout(r, ms);
  });
}
