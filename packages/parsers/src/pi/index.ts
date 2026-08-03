/**
 * pi harness JSONL parser.
 *
 * Layout: ~/.pi/agent/sessions/<encoded-cwd>/<TS>_<UUID>.jsonl
 *   (encoded-cwd mirrors Claude Code's scheme — `/` → `-`, wrapped in `--…--`).
 *
 * Line shapes (every line carries top-level { type, id?, parentId?, timestamp }):
 *   { type: "session", version, id: <session-uuid>, timestamp, cwd }
 *   { type: "model_change", provider, modelId, ... }      // session-level current model
 *   { type: "thinking_level_change", thinkingLevel, ... } // noise
 *   { type: "custom", customType, data, ... }             // UI state — noise
 *   { type: "message", message: { role, ... } }
 *
 * `message.role` is one of:
 *   user        { role, content: [{ type: "text", text }], timestamp }
 *   assistant   { role, content: [ thinking | text | toolCall ], api, provider,
 *                 model, usage: { input, output, cacheRead, cacheWrite,
 *                 totalTokens, cost, cacheWrite1h }, stopReason, timestamp }
 *               toolCall block → { type: "toolCall", id, name, arguments }
 *   toolResult  { role, toolCallId, toolName, content: [{ type, text }], isError, timestamp }
 *
 * Tool activity lives in assistant `toolCall` content blocks, paired back to
 * their `toolResult` line by toolCallId — yielding ToolCallDrafts (never
 * RawEvents). PRIVACY: arguments/results are reduced to sha256 hashes + byte
 * sizes on this device (see @modelstat/parsers/tool-hash); raw payloads never
 * leave. The session's per-token cost pi already computed is ignored — the
 * server prices from token counts like every other agent.
 */

import { createReadStream } from "node:fs";
import { createInterface } from "node:readline";
import type { Provider, RawEvent } from "@modelstat/core";
import { detectEventReferences, redact, sourceEventId } from "@modelstat/core";
import { guessRepoSlugFromPath } from "../git.js";
import { extractLocalToolContext, extractToolAction } from "../tool-action/index.js";
import {
  fallbackCallId,
  hashArgs,
  jsonBytes,
  splitObservedToolName,
  toolIdentity,
} from "../tool-hash/index.js";
import {
  type LocalToolContext,
  PARSER_EVENT_CHUNK,
  type ParseResult,
  type ParserContext,
  type ToolCallDraft,
} from "../types.js";

type PiContentBlock =
  | { type: "text"; text?: string }
  | { type: "thinking"; thinking?: string }
  | { type: "toolCall"; id?: string; name?: string; arguments?: unknown };

interface PiUsage {
  input?: number;
  output?: number;
  cacheRead?: number;
  cacheWrite?: number;
}

interface PiMessageLine {
  type: "message";
  timestamp: string;
  message: {
    role: "user" | "assistant" | "toolResult";
    content?: PiContentBlock[];
    // assistant-only
    provider?: string;
    model?: string;
    usage?: PiUsage;
    // toolResult-only
    toolCallId?: string;
    toolName?: string;
    isError?: boolean;
  };
}

interface PiSessionLine {
  type: "session";
  id?: string;
  timestamp?: string;
  cwd?: string | null;
}

interface PiModelChangeLine {
  type: "model_change";
  provider?: string;
  modelId?: string;
}

type PiLine =
  | PiSessionLine
  | PiModelChangeLine
  | PiMessageLine
  | { type: string; [k: string]: unknown };

/** Map pi's free-form provider string onto the closed PROVIDERS enum.
 * Substring matches (not equality) on purpose: pi sometimes records a model
 * name in the provider slot (e.g. `pi-claude-cli`, `claude-opus-4-8`). */
function mapProvider(raw: string | null | undefined): Provider {
  if (!raw) return "unknown";
  const p = raw.toLowerCase();
  if (p.includes("anthropic") || p.includes("claude")) return "anthropic";
  if (p.includes("openai") || p.includes("gpt") || p.includes("codex")) return "openai";
  if (p.includes("google") || p.includes("gemini")) return "google";
  if (p.includes("deepseek")) return "deepseek";
  if (p.includes("moonshot") || p.includes("kimi")) return "moonshot";
  if (p.includes("mistral")) return "mistral";
  if (p.includes("xai") || p.includes("grok")) return "xai";
  if (p.includes("ollama")) return "ollama_local";
  return "unknown";
}

/** pi session filename is `<ISO-ish-TS>_<session-uuid>.jsonl`. */
export function deriveSessionIdFromPiPath(path: string): string | null {
  const m = /_([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$/.exec(path);
  return m ? (m[1] ?? null) : null;
}

/** Join every `text` block's text (thinking/toolCall blocks dropped — they're
 * structured or private). Shared by the excerpt + reference passes below. */
function joinTextBlocks(content: PiContentBlock[] | undefined): string {
  if (!Array.isArray(content)) return "";
  const parts: string[] = [];
  for (const block of content) {
    if (block.type === "text" && typeof block.text === "string") parts.push(block.text);
  }
  return parts.join(" ");
}

/** Redacted, ≤320-char text excerpt from a pi message's content. Code fences
 * stripped, then scrubbed via @modelstat/core/redact. */
function extractExcerpt(content: PiContentBlock[] | undefined): string | undefined {
  let text = joinTextBlocks(content);
  if (!text) return undefined;
  text = text.replace(/```[\s\S]*?```/g, " ").replace(/`[^`]*`/g, " ");
  text = text.replace(/\s+/g, " ").trim();
  if (!text) return undefined;
  const cleaned = redact(text).text;
  const truncated = cleaned.slice(0, 320);
  return truncated.length > 0 ? truncated : undefined;
}

/** Full-length TEXT join (no truncation/code-strip) for public-reference
 * detection. Capped so a giant turn can't make the regex scan unbounded. Not
 * redacted — detectEventReferences pulls only public ref shapes, never raw text. */
function collectRefText(content: PiContentBlock[] | undefined): string {
  const text = joinTextBlocks(content);
  return text.length > 64_000 ? text.slice(0, 64_000) : text;
}

export async function parsePiSession(ctx: ParserContext): Promise<ParseResult> {
  // Streaming mode (ctx.onEvents set): `events` stays empty and parsed events
  // leave through the sink in PARSER_EVENT_CHUNK-sized chunks, so the working
  // set is bounded no matter how large the transcript is.
  const events: RawEvent[] = [];
  const toolCalls: ToolCallDraft[] = [];
  const scriptContexts: LocalToolContext[] = [];
  /** toolCall id → draft awaiting its toolResult (pairing is file-local). */
  const pendingByCallId = new Map<string, ToolCallDraft>();
  let chunk: RawEvent[] = [];
  let emitted = 0;
  const emit = async (e: RawEvent): Promise<void> => {
    emitted += 1;
    if (!ctx.onEvents) {
      events.push(e);
      return;
    }
    chunk.push(e);
    if (chunk.length >= PARSER_EVENT_CHUNK) {
      const full = chunk;
      chunk = [];
      await ctx.onEvents(full);
    }
  };
  let rawLines = 0;
  let skipped = 0;
  let bytePos = 0;
  const startOffset = ctx.byteOffsetStart ?? 0;

  const stream = createReadStream(ctx.sourceFile, { encoding: "utf8", start: startOffset });
  const rl = createInterface({ input: stream, crlfDelay: Infinity });

  // Session id comes from the filename (canonical) with a fallback to the
  // `session` line's own id for atypically-named files.
  let sessionId: string | null = deriveSessionIdFromPiPath(ctx.sourceFile);
  let cwd: string | null = null;
  // Last model_change provider/model — the session-level default an assistant
  // message inherits when it doesn't carry its own.
  let lastProvider: string | null = null;
  let lastModel: string | null = null;

  for await (const line of rl) {
    const byteLen = Buffer.byteLength(line, "utf8") + 1; // +1 for the stripped \n
    const offsetAtLineStart = startOffset + bytePos;
    bytePos += byteLen;
    rawLines += 1;
    if (!line.trim()) {
      skipped += 1;
      continue;
    }

    let obj: PiLine;
    try {
      obj = JSON.parse(line) as PiLine;
    } catch {
      skipped += 1;
      continue;
    }

    if (obj.type === "session") {
      const s = obj as PiSessionLine;
      if (!sessionId && s.id) sessionId = s.id;
      cwd = s.cwd ?? cwd;
      continue;
    }

    if (obj.type === "model_change") {
      const mc = obj as PiModelChangeLine;
      lastProvider = mc.provider ?? lastProvider;
      lastModel = mc.modelId ?? lastModel;
      continue;
    }

    if (obj.type !== "message") {
      skipped += 1;
      continue;
    }

    const ml = obj as PiMessageLine;
    const m = ml.message;
    if (!m?.role || !ml.timestamp || !sessionId) {
      skipped += 1;
      continue;
    }

    if (m.role === "assistant") {
      const usage = m.usage ?? {};
      if (m.model) lastModel = m.model;
      if (m.provider) lastProvider = m.provider;
      const provider = mapProvider(m.provider ?? lastProvider);
      const model = m.model ?? lastModel ?? null;
      const eventId = sourceEventId(ctx.deviceId, ctx.sourceFile, offsetAtLineStart);
      const slug = guessRepoSlugFromPath(cwd);
      const excerpt = extractExcerpt(m.content);
      const refs = detectEventReferences(collectRefText(m.content));

      // Per-call toolCall extraction + the aggregate identity → count map
      // carried by this assistant event. Drafts ride ParseResult.toolCalls —
      // they never become RawEvents (the pipeline would count them as turns).
      const aggregate: Record<string, number> = {};
      const blocks = Array.isArray(m.content) ? m.content : [];
      let callIndex = 0;
      for (const block of blocks) {
        if (block.type !== "toolCall") continue;
        const index = callIndex;
        callIndex += 1;
        const observed = typeof block.name === "string" ? block.name.trim() : "";
        if (!observed) continue;
        const { server, name } = splitObservedToolName(observed);
        const input = block.arguments;
        const hashes = hashArgs(input);
        const rawId = block.id;
        const externalCallId =
          typeof rawId === "string" && rawId.trim() !== ""
            ? rawId.trim().slice(0, 120)
            : fallbackCallId(eventId, index);
        const local = extractLocalToolContext({ server, name, input, cwd });
        if (local) scriptContexts.push({ external_call_id: externalCallId, ...local });
        const draft: ToolCallDraft = {
          external_call_id: externalCallId,
          session_id: sessionId,
          source_event_id: eventId,
          agent: "pi",
          server,
          name,
          turn_index: null,
          call_index: index,
          started_at: ml.timestamp,
          ended_at: null,
          status: "unknown",
          args_hash: hashes.args_hash,
          signature_hash: hashes.signature_hash,
          args_bytes: hashes.args_bytes,
          result_bytes: 0,
          model,
          action: extractToolAction({ server, name, input, cwd }),
        };
        const identity = toolIdentity(server, name);
        aggregate[identity] = (aggregate[identity] ?? 0) + 1;
        toolCalls.push(draft);
        if (typeof rawId === "string" && rawId) pendingByCallId.set(rawId, draft);
      }

      await emit({
        source_event_id: eventId,
        ts: ml.timestamp,
        kind: "assistant_message",
        agent: "pi",
        provider,
        model,
        session_id: sessionId,
        turn_index: null,
        parent_event_id: null,
        cwd,
        git: slug
          ? {
              remote_url: null,
              remote_host: slug.includes("/") ? "github.com" : null,
              remote_slug: slug,
              branch: null,
            }
          : null,
        tokens: {
          input: usage.input ?? 0,
          output: usage.output ?? 0,
          cache_creation: usage.cacheWrite ?? 0,
          cache_read: usage.cacheRead ?? 0,
          reasoning: 0,
        },
        duration_ms: null,
        tool_calls: aggregate,
        files_touched: [],
        ...(excerpt ? { content_excerpt: excerpt } : {}),
        ...(refs ? { references: refs } : {}),
        source_file: ctx.sourceFile,
        source_byte_offset: offsetAtLineStart,
        pricing_mode: ctx.pricingMode ?? "unknown",
      });
      continue;
    }

    if (m.role === "toolResult") {
      // Pair back to the pending draft (matched by toolCallId). Only the
      // hash-safe metadata is kept: timestamp, error flag, result byte size.
      const ref = m.toolCallId;
      if (typeof ref === "string") {
        const draft = pendingByCallId.get(ref);
        if (draft) {
          pendingByCallId.delete(ref);
          draft.ended_at = ml.timestamp;
          draft.status = m.isError === true ? "error" : "success";
          draft.result_bytes = jsonBytes(m.content);
        }
      }
      skipped += 1;
      continue;
    }

    // user message
    const excerpt = extractExcerpt(m.content);
    const refs = detectEventReferences(collectRefText(m.content));
    await emit({
      source_event_id: sourceEventId(ctx.deviceId, ctx.sourceFile, offsetAtLineStart),
      ts: ml.timestamp,
      kind: "user_message",
      agent: "pi",
      provider: mapProvider(lastProvider),
      model: lastModel,
      session_id: sessionId,
      turn_index: null,
      parent_event_id: null,
      cwd,
      git: null,
      tokens: null,
      duration_ms: null,
      tool_calls: {},
      files_touched: [],
      ...(excerpt ? { content_excerpt: excerpt } : {}),
      ...(refs ? { references: refs } : {}),
      source_file: ctx.sourceFile,
      source_byte_offset: offsetAtLineStart,
      pricing_mode: ctx.pricingMode ?? "unknown",
    });
  }

  if (ctx.onEvents && chunk.length > 0) await ctx.onEvents(chunk);

  return {
    events,
    toolCalls,
    scriptContexts,
    stats: { rawLines, emittedEvents: emitted, skipped },
    sourceFile: ctx.sourceFile,
  };
}
