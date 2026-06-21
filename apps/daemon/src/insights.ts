/**
 * Per-session insights — the daemon side of Feature 1 ("live per-session
 * insights").
 *
 * After a session scan the daemon asks the server for that session's rolled-up
 * insights (tokens, $ assigned, taxonomy nodes detected, status) via the
 * unified MCP `session_insights` tool, and CACHES the payload under
 * `~/.modelstat/sessions/<sessionId>.json`. The always-on `modelstat
 * statusline` command reads ONLY this cache (never the network), so the
 * status line is instant and offline-tolerant.
 *
 * The server's enrichment is async (tokens+cost land in ~2s, taxonomy follows
 * the LLM tick), so a freshly-scanned session is often `analyzing` for a few
 * seconds. We short-poll a small, bounded number of times so the cache lands
 * `ready` without spinning.
 */
import { mkdir, rename, writeFile } from "node:fs/promises";
import { request } from "undici";
import { createLogger } from "@modelstat/daemon-core/logger";
import { state } from "./config.js";
import { homePath } from "./paths.js";

const logger = createLogger("daemon.insights");

/** One taxonomy node detected for a session — the chips the widget +
 * statusline render. Mirrors the server's `session_insights` shape; kept
 * permissive so a server-side field addition can't break the cache. */
export interface InsightTaxonomyNode {
  id: string;
  name: string;
  path?: string;
  root_key?: string;
  color?: string | null;
  emoji?: string | null;
}

/** Parsed `session_insights` payload as cached on disk. Optional/loose on
 * purpose — the authoritative schema is the server's; the statusline reads
 * defensively. */
export interface SessionInsights {
  status: "ready" | "analyzing" | "not_ingested" | string;
  segments_pending?: number;
  session_ids?: string[];
  missing_session_ids?: string[];
  started_at?: string | null;
  ended_at?: string | null;
  segment_count?: number;
  tokens?: {
    input?: number;
    output?: number;
    cache_read?: number;
    cache_creation?: number;
    reasoning?: number;
    total?: number;
  };
  cost_usd?: string | number | null;
  taxonomy_nodes?: InsightTaxonomyNode[];
  /** Stamped by the daemon when it wrote the cache (not from the server). */
  cached_at?: string;
}

/** Directory holding the per-session insight cache files. */
function sessionsDir(): string {
  return homePath("sessions");
}

/** Absolute path to one session's cached insights. The id is path-segment
 * safe (Claude/Codex session ids are UUIDs), but encode anyway so an unusual
 * id can never escape the directory. */
export function sessionInsightsPath(sessionId: string): string {
  return homePath("sessions", `${encodeURIComponent(sessionId)}.json`);
}

/**
 * Call the server's unified MCP `session_insights` tool for a session chain.
 * Returns the parsed payload, or null when the daemon isn't enrolled / the
 * call fails / the body isn't the expected JSON-in-text shape (the caller
 * treats null as "nothing to cache this round").
 */
export async function fetchSessionInsights(
  sessionIds: string[],
  opts: { eager?: boolean } = {},
): Promise<SessionInsights | null> {
  const bearer = state.bearer;
  if (!bearer) return null;
  if (sessionIds.length === 0) return null;
  try {
    const res = await request(`${state.apiUrl}/v1/mcp/call`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${bearer}` },
      body: JSON.stringify({
        name: "session_insights",
        arguments: { session_ids: sessionIds, eager: opts.eager === true },
      }),
    });
    if (res.statusCode >= 300) {
      await res.body.dump();
      return null;
    }
    const body = (await res.body.json()) as {
      content?: Array<{ type?: string; text?: string }>;
      isError?: boolean;
    };
    if (body.isError) return null;
    // The MCP envelope carries the insights JSON as the first text block.
    const text = body.content?.find((c) => c.type === "text")?.text ?? body.content?.[0]?.text;
    if (!text) return null;
    return JSON.parse(text) as SessionInsights;
  } catch (e) {
    logger.warn(`session_insights fetch failed: ${(e as Error).message}`);
    return null;
  }
}

/** Atomically write a session's insights to the cache (tmp + rename), stamping
 * `cached_at`. Best-effort — a write failure never sinks a scan. */
export async function cacheSessionInsights(
  sessionId: string,
  insights: SessionInsights,
): Promise<void> {
  const path = sessionInsightsPath(sessionId);
  const payload = { ...insights, cached_at: new Date().toISOString() };
  try {
    await mkdir(sessionsDir(), { recursive: true });
    const tmp = `${path}.tmp`;
    await writeFile(tmp, JSON.stringify(payload));
    await rename(tmp, path);
  } catch (e) {
    logger.warn(`session insights cache write failed for ${sessionId}: ${(e as Error).message}`);
  }
}

/** Bounded short-poll while the server is still `analyzing` a freshly-scanned
 * session. Small + capped: the server enriches in seconds and the statusline
 * happily renders an interim `analyzing` cache, so this never blocks for long. */
const POLL_DELAYS_MS = [1200, 2000, 3000, 4000] as const;

/**
 * Fetch + cache one session chain's insights, eagerly prioritising the
 * server's enrichment, and short-poll a few times while it's `analyzing` so
 * the cached payload converges to `ready`. Caches every interim result too, so
 * the statusline shows progress (`analyzing` → numbers) without waiting for the
 * terminal state. Best-effort throughout.
 *
 * @param sessionIds the compaction chain (or a single id). The cache is keyed
 *   by the FIRST id — the one Claude Code reports as the live `session_id`,
 *   which is what `modelstat statusline` looks up.
 */
export async function refreshSessionInsights(sessionIds: string[]): Promise<void> {
  if (sessionIds.length === 0) return;
  const cacheKey = sessionIds[0] as string;

  let insights = await fetchSessionInsights(sessionIds, { eager: true });
  if (insights) await cacheSessionInsights(cacheKey, insights);

  for (const delay of POLL_DELAYS_MS) {
    if (!insights || insights.status !== "analyzing") break;
    await sleep(delay);
    // Re-poll without re-prioritising — the priority signal already fired.
    const next = await fetchSessionInsights(sessionIds, { eager: false });
    if (next) {
      insights = next;
      await cacheSessionInsights(cacheKey, next);
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
