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
  | { type: "drop"; reason: string }
  | { type: "reauth" }
  | { type: "backoff"; delayMs: number };

/**
 * Classify an HTTP status into the next action to take. Shared across
 * daemons so "what happens on 401" is answered in exactly one place.
 */
export function classifyStatus(status: number, attempt: number): UploadDecision {
  if (status >= 200 && status < 300) return { type: "commit" };
  if (status === 400 || status === 422) return { type: "drop", reason: `http_${status}` };
  if (status === 401 || status === 403) return { type: "reauth" };
  // 426 = daemon too old (the server's min-version gate). Transient by nature:
  // the daemon auto-updates via its heartbeat, after which the retry succeeds —
  // so back off and HOLD the batch, never drop it. (Dropping logged an error and
  // re-spun the same data every scan cycle until the update landed.)
  if (status === 408 || status === 426 || status === 429 || status >= 500) {
    return { type: "backoff", delayMs: expBackoff(attempt) };
  }
  // Any other 4xx is treated as permanent — drop so we don't spin.
  return { type: "drop", reason: `http_${status}` };
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
      if (decision.type === "drop") {
        const text = await res.text().catch(() => "");
        this.opts.logger.error("ingest dropped", {
          status: res.status,
          reason: decision.reason,
          body: text.slice(0, 500),
        });
        // A 400/422 (or other permanent 4xx) — the server will never accept this
        // payload. Permanent so the caller quarantines it, not holds it.
        return { kind: "drop", reason: decision.reason, permanent: true };
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
      // backoff — drain the unread body first (see reauth note). A
      // backfill that hits sustained 429s loops here repeatedly, so an
      // un-cancelled body per attempt accumulates on the heap.
      await res.body?.cancel().catch(() => {});
      this.opts.logger.warn("ingest backoff", {
        status: res.status,
        delay_ms: decision.delayMs,
        attempt,
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
  // `permanent`: the server will NEVER accept this exact batch (400/422 — a
  // malformed/un-encodable payload), so the caller must QUARANTINE it (skip past
  // it + alert) rather than block the newest-first scan behind it forever.
  // `!permanent` = transient (no token / reauth failed / 5xx-exhausted / network)
  // → HOLD the batch and retry next cycle; never drop good data on a blip.
  | { kind: "drop"; reason: string; permanent: boolean };

function sleep(ms: number): Promise<void> {
  return new Promise((r) => {
    setTimeout(r, ms);
  });
}
