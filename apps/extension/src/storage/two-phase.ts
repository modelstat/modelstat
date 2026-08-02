/**
 * Two-phase message commit.
 *
 * Providers emit message data across multiple channels at different
 * times: DOM shows text as it streams, network SSE yields token usage
 * AFTER the stream ends. We buffer both into a single `pending` row
 * keyed by (host, messageId), and finalise when:
 *
 *   - the commit window closes (MESSAGE_FINALISE_WINDOW_MS), OR
 *   - the stream has ended AND the DOM has been stable for
 *     MESSAGE_FINALISE_DOM_QUIET_MS
 *
 * On finalisation we:
 *   1. Tokenize `text` if the network didn't give us usage
 *   2. Compute cost via the pricing table
 *   3. Insert a StoredEvent (idempotent — the `source_event_id`
 *      equals `${host}:${messageId}` and is a unique index)
 *   4. Update the SessionRollup
 *   5. Delete the pending row
 */

import type { Agent } from "@modelstat/core/enums";
import {
  EAGER_FINALISE_QUIET_MS,
  MESSAGE_FINALISE_DOM_QUIET_MS,
  MESSAGE_FINALISE_WINDOW_MS,
} from "@/common/config.js";
import { createLogger } from "@/common/logger.js";
import type { NetworkMessage, NetworkScalar } from "@/interpreter/network.js";
import type { DomEventPayload } from "@/interpreter/runtime-msgs.js";
import { db, type PendingMessage, type StoredEvent, type SessionRollup } from "./db.js";

const log = createLogger("two-phase");

type Emit = (event: StoredEvent) => Promise<void>;

type RequestTokenize = (
  tokenizerBinding: { default: string; byModel?: Record<string, string> },
  model: string | null,
  text: string,
) => Promise<{ tokens: number; name: string; accuracy: "exact" | "estimated" }>;

export type CommitterCtx = {
  agent: Agent;
  vendor: string;
  host: string;
  tokenizerBinding: { default: string; byModel?: Record<string, string> };
  requestTokenize: RequestTokenize;
  onEvent: Emit;
};

const pendingKey = (host: string, messageId: string): string => `${host}::${messageId}`;

export async function ingestNetworkMessage(
  msg: NetworkMessage,
  ctx: CommitterCtx,
  now: number = Date.now(),
): Promise<void> {
  const key = pendingKey(msg.host, msg.messageId);
  await db().transaction("rw", db().pending, async () => {
    const existing = await db().pending.get(key);
    const merged: PendingMessage = existing
      ? {
          ...existing,
          text: msg.text ?? existing.text,
          model: msg.model ?? existing.model,
          usage: {
            input: msg.usage.input ?? existing.usage.input,
            output: msg.usage.output ?? existing.usage.output,
            cache_creation: msg.usage.cache_creation ?? existing.usage.cache_creation,
            cache_read: msg.usage.cache_read ?? existing.usage.cache_read,
            reasoning: msg.usage.reasoning ?? existing.usage.reasoning,
          },
          lastUpdatedAt: now,
        }
      : {
          key,
          host: msg.host,
          messageId: msg.messageId,
          role: msg.role,
          text: msg.text,
          model: msg.model,
          conversationId: null,
          usage: msg.usage,
          firstSeenAt: now,
          lastUpdatedAt: now,
          streamEnded: false,
          domStableSince: null,
          finalised: false,
        };
    await db().pending.put(merged);
  });
}

export async function ingestDomEvent(payload: DomEventPayload, ctx: CommitterCtx): Promise<void> {
  if (payload.source !== "dom-observe") return;
  const key = pendingKey(payload.host, payload.messageId);
  const now = Date.now();
  await db().transaction("rw", db().pending, async () => {
    const existing = await db().pending.get(key);
    const merged: PendingMessage = existing
      ? {
          ...existing,
          role: payload.role ?? existing.role,
          text: payload.text ?? existing.text,
          conversationId: payload.conversationId ?? existing.conversationId,
          lastUpdatedAt: now,
          domStableSince: existing.domStableSince ?? now,
        }
      : {
          key,
          host: payload.host,
          messageId: payload.messageId,
          role: payload.role,
          text: payload.text,
          model: null,
          conversationId: payload.conversationId,
          usage: {
            input: null,
            output: null,
            cache_creation: null,
            cache_read: null,
            reasoning: null,
          },
          firstSeenAt: now,
          lastUpdatedAt: now,
          streamEnded: false,
          domStableSince: now,
          finalised: false,
        };
    await db().pending.put(merged);
  });
}

export async function ingestScalar(scalar: NetworkScalar): Promise<void> {
  // Scalars (conversation_id, model) just update all un-finalised pending
  // rows for this host — cheap, bounded.
  if (scalar.field === "model") {
    await db()
      .pending.where({ host: scalar.host, finalised: false })
      .modify({ model: scalar.value });
  } else if (scalar.field === "conversation_id") {
    await db()
      .pending.where({ host: scalar.host, finalised: false })
      .modify({ conversationId: scalar.value });
  }
}

/** Called by a periodic SW alarm — checks every pending row, finalises
 * those ready. `eager` (first-impression fast path only) finalises a
 * stream-ended message as soon as it has briefly settled, without waiting on
 * the DOM-quiet anchor (which network-only captures never set) or the 30s
 * window — so the very first session ships in seconds. Steady-state sweeps
 * pass no opts and behave exactly as before. */
export async function sweepFinalise(
  ctx: CommitterCtx,
  opts?: { eager?: boolean },
): Promise<number> {
  const now = Date.now();
  const ready: PendingMessage[] = [];
  await db()
    .pending.where("finalised")
    .equals(0 as unknown as number)
    .each((row) => {
      const ageMs = now - row.firstSeenAt;
      const quietMs = row.domStableSince ? now - row.domStableSince : 0;
      const windowElapsed = ageMs >= MESSAGE_FINALISE_WINDOW_MS;
      const streamAndQuiet =
        row.streamEnded &&
        (opts?.eager
          ? now - row.lastUpdatedAt >= EAGER_FINALISE_QUIET_MS
          : quietMs >= MESSAGE_FINALISE_DOM_QUIET_MS);
      if (windowElapsed || streamAndQuiet) ready.push(row);
    });
  for (const row of ready) {
    try {
      await finaliseRow(row, ctx);
    } catch (e) {
      log.warn("finalise failed", row.key, e);
    }
  }
  return ready.length;
}

async function finaliseRow(row: PendingMessage, ctx: CommitterCtx): Promise<void> {
  const text = row.text ?? "";
  const model = row.model;

  // Fill in missing usage via local tokenizer when the provider didn't
  // report it. Input/output heuristic: the *text* we captured is the
  // assistant output (for role=assistant) or user input (role=user).
  let input = row.usage.input ?? 0;
  let output = row.usage.output ?? 0;
  let tokenizerName = "unknown";
  let tokenizerAccuracy: "exact" | "estimated" = "estimated";
  if (text && ((row.role === "assistant" && output === 0) || (row.role === "user" && input === 0))) {
    try {
      const r = await ctx.requestTokenize(ctx.tokenizerBinding, model, text);
      tokenizerName = r.name;
      tokenizerAccuracy = r.accuracy;
      if (row.role === "assistant") output = r.tokens;
      else input = r.tokens;
    } catch (e) {
      log.warn("tokenize failed", e);
    }
  }

  // OpenAI reports input_tokens / output_tokens INCLUSIVE of their cached /
  // reasoning subsets; the server bills input + cache_read + output + reasoning
  // as DISJOINT line items, so subtract the subsets for OpenAI — otherwise cached
  // and reasoning tokens are paid for twice (G7/G8) in the ingested tokens this
  // StoredEvent feeds the server.
  // Mirrors the Codex parser. Anthropic already reports these buckets disjoint.
  // No-op when usage wasn't captured (subtracting 0) or text was tokenized.
  if (ctx.vendor === "openai") {
    input = Math.max(0, input - (row.usage.cache_read ?? 0));
    output = Math.max(0, output - (row.usage.reasoning ?? 0));
  }

  const conversationId = row.conversationId ?? row.messageId;
  const session_id = `${ctx.agent}::${conversationId}`;
  const source_event_id = `${row.host}::${row.messageId}`;
  const ts = new Date(row.lastUpdatedAt).toISOString();

  const event: StoredEvent = {
    source_event_id,
    ts,
    agent: ctx.agent,
    vendor: ctx.vendor,
    model,
    session_id,
    role: row.role,
    input_tokens: input,
    output_tokens: output,
    cache_creation_tokens: row.usage.cache_creation ?? 0,
    cache_read_tokens: row.usage.cache_read ?? 0,
    reasoning_tokens: row.usage.reasoning ?? 0,
    tokenizer_name: tokenizerName,
    tokenizer_accuracy: tokenizerAccuracy,
    duration_ms: row.lastUpdatedAt - row.firstSeenAt,
    host: row.host,
    summary: null,
    category: null,
    synced: 0,
  };
  await ctx.onEvent(event);

  // Update rollup — aggregate per (agent, conversation_id).
  await db().transaction("rw", [db().sessions, db().events, db().pending], async () => {
    const existing = await db().sessions.get(session_id);
    const rollup: SessionRollup = existing
      ? {
          ...existing,
          ended_at: ts,
          model: model ?? existing.model,
          message_count: existing.message_count + 1,
          tokens_input: existing.tokens_input + event.input_tokens,
          tokens_output: existing.tokens_output + event.output_tokens,
          tokens_cache_creation: existing.tokens_cache_creation + event.cache_creation_tokens,
          tokens_cache_read: existing.tokens_cache_read + event.cache_read_tokens,
          tokens_reasoning: existing.tokens_reasoning + event.reasoning_tokens,
          updated_at: ts,
        }
      : {
          session_id,
          agent: ctx.agent,
          vendor: ctx.vendor,
          model,
          conversation_id: conversationId,
          host: row.host,
          started_at: ts,
          ended_at: ts,
          message_count: 1,
          tokens_input: event.input_tokens,
          tokens_output: event.output_tokens,
          tokens_cache_creation: event.cache_creation_tokens,
          tokens_cache_read: event.cache_read_tokens,
          tokens_reasoning: event.reasoning_tokens,
          summary: null,
          category: null,
          updated_at: ts,
        };
    await db().sessions.put(rollup);
    // Idempotent events insert — use source_event_id as the functional key
    // and skip-if-seen.
    const existingEvent = await db().events.where("source_event_id").equals(source_event_id).first();
    if (!existingEvent) await db().events.add(event);
    await db().pending.delete(row.key);
  });
}

export async function markStreamEnded(host: string, messageId: string): Promise<void> {
  await db()
    .pending.where({ host, messageId, finalised: false })
    .modify({ streamEnded: true, lastUpdatedAt: Date.now() });
}
