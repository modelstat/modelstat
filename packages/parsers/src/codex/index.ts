/**
 * Codex CLI rollout parser.
 *
 * Layout: ~/.codex/sessions/YYYY/MM/DD/rollout-<TS>-<UUID>.jsonl
 * Event stream with types:
 *   { type: "session_meta", id, ... }            // newer CLIs nest under payload: { id, cwd, ... }
 *   { type: "turn_context", cwd, model, ... }    // newer CLIs nest under payload: { cwd, model, ... }
 *   { type: "response_item", role: "user"|"assistant", ... }
 *   { type: "event_msg", payload: { type: "user_message"|"agent_message"|"token_count", ... } }
 *
 * Token accounting lives inside event_msg with payload.type === "token_count":
 *   { input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens, total_tokens, model_context_window }
 *
 * Tool activity lives in response_item lines (payload.type):
 *   function_call          { name, arguments: <JSON string>, call_id }
 *   local_shell_call       { call_id?, id?, status, action: { type: "exec", command: string[] | string, ... } }
 *   custom_tool_call       { name, input: <free-text string>, call_id, status }
 *   mcp_tool_call          { server?, tool? | name, arguments?, call_id }
 *   *_output               { call_id, output: <string> } — paired by call_id
 * These produce ToolCallDrafts (never RawEvents); the aggregate identity→count
 * map attaches to the next emitted assistant event of the same session.
 * PRIVACY: arguments/outputs are reduced to hashes + byte sizes on this device
 * (see @modelstat/parsers/tool-hash); the action decomposition is filled by
 * the on-device extractor in a later phase. Raw payloads never leave.
 */
import { createReadStream } from "node:fs";
import { createInterface } from "node:readline";
import type { RawEvent } from "@modelstat/core";
import { sourceEventId } from "@modelstat/core";
import { guessRepoSlugFromPath } from "../git.js";
import { extractLocalToolContext, extractToolAction } from "../tool-action/index.js";
import {
  fallbackCallId,
  hashArgs,
  jsonBytes,
  normalizeToolName,
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

interface CodexTokenCount {
  type: "token_count";
  input_tokens?: number;
  cached_input_tokens?: number;
  output_tokens?: number;
  reasoning_output_tokens?: number;
  total_tokens?: number;
  model_context_window?: number;
}

interface CodexEventMsg {
  type: "event_msg";
  payload: { type: string; [k: string]: unknown };
  timestamp?: string;
  id?: string;
}

interface CodexSessionMeta {
  type: "session_meta";
  id?: string;
  created_at?: string;
  timestamp?: string;
  /** Newer rollout format nests the meta under payload. */
  payload?: { id?: string; [k: string]: unknown };
}

interface CodexTurnContext {
  type: "turn_context";
  cwd?: string;
  model?: string;
  effort?: string;
  timestamp?: string;
  /** Newer rollout format nests the context under payload. */
  payload?: { cwd?: string; model?: string; [k: string]: unknown };
}

interface CodexResponseItem {
  type: "response_item";
  payload?: { type?: string; [k: string]: unknown };
  timestamp?: string;
}

type CodexLine =
  | CodexSessionMeta
  | CodexTurnContext
  | CodexEventMsg
  | CodexResponseItem
  | { type: string; [k: string]: unknown };

/** response_item payload types that open a tool call. */
const TOOL_CALL_PAYLOAD_TYPES: ReadonlySet<string> = new Set([
  "function_call",
  "local_shell_call",
  "custom_tool_call",
  "mcp_tool_call",
]);

/** Their output counterparts, paired back to the call by call_id. */
const TOOL_CALL_OUTPUT_PAYLOAD_TYPES: ReadonlySet<string> = new Set([
  "function_call_output",
  "local_shell_call_output",
  "custom_tool_call_output",
  "mcp_tool_call_output",
]);

/** Codex names "the shell" differently across versions (exec_command in
 * current CLIs, shell/local_shell_call in older ones). All map to the
 * wire name `shell`. */
const SHELL_TOOL_NAMES: ReadonlySet<string> = new Set([
  "shell",
  "local_shell_call",
  "exec_command",
  "run_terminal_cmd",
]);

export function deriveSessionIdFromRolloutPath(path: string): string | null {
  const m =
    /rollout-[0-9T-]+-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$/.exec(
      path,
    );
  return m ? (m[1] ?? null) : null;
}

interface ExtractedToolCall {
  /** codex call_id (or id); null → caller derives a deterministic fallback. */
  callId: string | null;
  /** Wire server: `builtin` or `mcp:<server>`. */
  server: string;
  /** Wire name (normalised; `shell` for shell-ish calls). */
  name: string;
  /** What gets hashed as args — parsed object when possible, else raw string. */
  input: unknown;
  /** The call line itself said it failed (status: "failed"). */
  failed: boolean;
}

function firstString(...values: unknown[]): string | null {
  for (const v of values) {
    if (typeof v === "string" && v) return v;
  }
  return null;
}

/** Pin one tool-call payload to the wire shape. Returns null when the
 * payload is too malformed to identify (no name at all). */
function extractToolCallPayload(pt: string, p: Record<string, unknown>): ExtractedToolCall | null {
  const callId = firstString(p.call_id, p.id);
  const failed = p.status === "failed";

  if (pt === "local_shell_call") {
    const action =
      p.action && typeof p.action === "object" ? (p.action as Record<string, unknown>) : null;
    return {
      callId,
      server: "builtin",
      name: "shell",
      input: action,
      failed,
    };
  }

  const observed = firstString(p.name, p.tool);
  if (!observed) return null;

  // function_call/mcp_tool_call carry `arguments` as a JSON-encoded string;
  // custom_tool_call carries free-text `input` (e.g. an apply_patch body)
  // that must be hashed verbatim, never parsed.
  let input: unknown = pt === "custom_tool_call" ? p.input : (p.arguments ?? p.input);
  if (typeof input === "string" && !input.trim()) input = undefined;
  if (pt !== "custom_tool_call" && typeof input === "string") {
    try {
      input = JSON.parse(input);
    } catch {
      // Not JSON — hash the raw string as-is.
    }
  }

  if (SHELL_TOOL_NAMES.has(observed)) {
    return {
      callId,
      server: "builtin",
      name: "shell",
      input,
      failed,
    };
  }

  if (pt === "mcp_tool_call" && typeof p.server === "string" && p.server) {
    return {
      callId,
      // Mirror splitObservedToolName's cap: 116 + the `mcp:` prefix ≤ 120.
      server: `mcp:${normalizeToolName(p.server).slice(0, 116)}`,
      name: normalizeToolName(observed),
      input,
      failed,
    };
  }

  // Handles both plain names (`update_plan` → builtin) and the
  // `mcp__<server>__<tool>` form some function_call lines use.
  const { server, name } = splitObservedToolName(observed);
  return { callId, server, name, input, failed };
}

/** Best-effort error sniffing on an output payload. Codex outputs are
 * usually opaque strings (exit codes buried in free text we don't read);
 * structured failure markers are honoured when present. */
function outputIndicatesError(p: Record<string, unknown>): boolean {
  const out = p.output ?? p.result;
  if (out && typeof out === "object" && !Array.isArray(out)) {
    const o = out as Record<string, unknown>;
    if (o.success === false || o.is_error === true) return true;
  }
  return false;
}

export async function parseCodexRollout(ctx: ParserContext): Promise<ParseResult> {
  // In streaming mode (ctx.onEvents set) `events` stays empty and
  // parsed events leave through the sink in PARSER_EVENT_CHUNK-sized
  // chunks — the parser's working set is bounded no matter how large
  // the rollout file is. See ParserContext.onEvents.
  const events: RawEvent[] = [];
  const toolCalls: ToolCallDraft[] = [];
  // Local-only raw command + cwd per shell call, for the agent's script-summary
  // pass. Never shipped (see LocalToolContext); returned on ParseResult.
  const scriptContexts: LocalToolContext[] = [];
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

  const stream = createReadStream(ctx.sourceFile, {
    encoding: "utf8",
    start: startOffset,
  });
  const rl = createInterface({ input: stream, crlfDelay: Infinity });

  let sessionId: string | null = deriveSessionIdFromRolloutPath(ctx.sourceFile);
  let cwd: string | null = null;
  let model: string | null = null;
  let turnIndex = 0;
  /** Last timestamp seen on any line — started_at fallback for the rare
   * response_item without its own ts. */
  let lastTs: string | null = null;
  /** Calls awaiting their *_output counterpart, keyed by call_id. */
  const openCalls = new Map<string, ToolCallDraft>();
  /** identity → count since the last assistant event; attaches to the next
   * one emitted in the same session (best-effort — dropped at EOF/session
   * switch when no assistant event follows; the drafts always ship). */
  let pendingToolAggregate: Record<string, number> = {};

  for await (const line of rl) {
    const byteLen = Buffer.byteLength(line, "utf8") + 1;
    const offsetAtLineStart = startOffset + bytePos;
    bytePos += byteLen;
    rawLines += 1;
    if (!line.trim()) {
      skipped += 1;
      continue;
    }

    let obj: CodexLine;
    try {
      obj = JSON.parse(line) as CodexLine;
    } catch {
      skipped += 1;
      continue;
    }

    const lineTs = (obj as { timestamp?: unknown }).timestamp;
    if (typeof lineTs === "string" && lineTs) lastTs = lineTs;

    if (obj.type === "session_meta") {
      const m = obj as CodexSessionMeta;
      const id = m.id ?? m.payload?.id ?? null;
      if (id && id !== sessionId) {
        // New session in the same file — don't let the aggregate map or
        // call_id pairing leak across the boundary.
        sessionId = id;
        pendingToolAggregate = {};
        openCalls.clear();
      }
      continue;
    }
    if (obj.type === "turn_context") {
      const t = obj as CodexTurnContext;
      cwd = t.cwd ?? t.payload?.cwd ?? cwd;
      model = t.model ?? t.payload?.model ?? model;
      continue;
    }

    if (obj.type === "response_item") {
      const r = obj as CodexResponseItem;
      const payload = r.payload;
      const pt = payload && typeof payload.type === "string" ? payload.type : null;

      if (pt && TOOL_CALL_PAYLOAD_TYPES.has(pt) && sessionId) {
        const extracted = extractToolCallPayload(pt, payload as Record<string, unknown>);
        if (!extracted) {
          skipped += 1;
          continue;
        }
        // started_at must be deterministic across re-parses so the same
        // event re-uploads idempotently: a wall-clock fallback would make
        // replays look like fresh events. So we take the line's own ts,
        // else the last ts seen in this file — and if neither exists we
        // SKIP the draft, mirroring claude-code, which never emits a
        // tool_use without a line timestamp. No current-time constructor
        // may reach started_at.
        const ts = r.timestamp ?? lastTs;
        if (!ts) {
          skipped += 1;
          continue;
        }
        const srcId = sourceEventId(ctx.deviceId, ctx.sourceFile, offsetAtLineStart);
        const { args_hash, signature_hash, args_bytes } = hashArgs(extracted.input);
        const externalCallId = (extracted.callId ?? fallbackCallId(srcId, 0)).slice(0, 120);
        // Stash the raw command + cwd locally (never shipped) so the Node agent
        // can summarise referenced script FILES into ToolAction.scripts. See
        // ParseResult.scriptContexts / LocalToolContext.
        const localCtx = extractLocalToolContext({
          server: extracted.server,
          name: extracted.name,
          input: extracted.input,
          cwd,
        });
        if (localCtx) scriptContexts.push({ external_call_id: externalCallId, ...localCtx });
        const draft: ToolCallDraft = {
          external_call_id: externalCallId,
          session_id: sessionId,
          source_event_id: srcId,
          agent: "codex_cli",
          server: extracted.server,
          name: extracted.name,
          turn_index: turnIndex,
          // Each response_item line carries exactly one call.
          call_index: 0,
          started_at: ts,
          ended_at: null,
          status: extracted.failed ? "error" : "unknown",
          args_hash,
          signature_hash,
          args_bytes,
          result_bytes: 0,
          model,
          action: extractToolAction({
            server: extracted.server,
            name: extracted.name,
            input: extracted.input,
            cwd,
          }),
        };
        toolCalls.push(draft);
        if (extracted.callId) openCalls.set(extracted.callId, draft);
        const identity = toolIdentity(extracted.server, extracted.name);
        pendingToolAggregate[identity] = (pendingToolAggregate[identity] ?? 0) + 1;
        continue;
      }

      if (pt && TOOL_CALL_OUTPUT_PAYLOAD_TYPES.has(pt)) {
        const p = payload as Record<string, unknown>;
        // Mirror the call-side key derivation (extractToolCallPayload uses
        // firstString(p.call_id, p.id)): an id-only call (e.g. some
        // local_shell_call shapes) is stored under its `id`, so the output
        // lookup must fall back to `id` too — otherwise an id-only-on-both-
        // sides pair never matches and ships status 'unknown'.
        const callId = firstString(p.call_id, p.id);
        const open = callId ? openCalls.get(callId) : undefined;
        if (!callId || !open) {
          // Output for a call we never saw (truncated file) — nothing to pair.
          skipped += 1;
          continue;
        }
        openCalls.delete(callId);
        open.ended_at = r.timestamp ?? lastTs ?? open.started_at;
        open.result_bytes = jsonBytes(p.output ?? p.result);
        if (open.status === "unknown") {
          open.status = outputIndicatesError(p) ? "error" : "success";
        }
        continue;
      }

      // message / reasoning / web_search_call (no call_id to pair) — not tool data.
      skipped += 1;
      continue;
    }

    if (obj.type === "event_msg") {
      const m = obj as CodexEventMsg;
      const ts = m.timestamp ?? new Date().toISOString();
      const payload = m.payload;
      if (!payload?.type) {
        skipped += 1;
        continue;
      }

      if (payload.type === "token_count") {
        const tk = payload as CodexTokenCount;
        if (!sessionId) {
          skipped += 1;
          continue;
        }
        const slug = guessRepoSlugFromPath(cwd);
        await emit({
          source_event_id: sourceEventId(ctx.deviceId, ctx.sourceFile, offsetAtLineStart),
          ts,
          kind: "assistant_message",
          agent: "codex_cli",
          provider: "openai",
          model,
          session_id: sessionId,
          turn_index: turnIndex,
          parent_event_id: null,
          cwd,
          git: slug
            ? {
                remote_url: null,
                remote_host: slug.includes("/") ? "github.com" : null,
                remote_slug: slug,
                branch: null,
                commit_sha: null,
              }
            : null,
          tokens: {
            input: tk.input_tokens ?? 0,
            output: tk.output_tokens ?? 0,
            cache_creation: 0,
            cache_read: tk.cached_input_tokens ?? 0,
            reasoning: tk.reasoning_output_tokens ?? 0,
          },
          duration_ms: null,
          tool_calls: pendingToolAggregate,
          files_touched: [],
          source_file: ctx.sourceFile,
          source_byte_offset: offsetAtLineStart,
        });
        pendingToolAggregate = {};
        turnIndex += 1;
        continue;
      }
      if (payload.type === "user_message") {
        if (!sessionId) {
          skipped += 1;
          continue;
        }
        await emit({
          source_event_id: sourceEventId(ctx.deviceId, ctx.sourceFile, offsetAtLineStart),
          ts,
          kind: "user_message",
          agent: "codex_cli",
          provider: "openai",
          model,
          session_id: sessionId,
          turn_index: null,
          parent_event_id: null,
          cwd,
          git: null,
          tokens: null,
          duration_ms: null,
          tool_calls: {},
          files_touched: [],
          source_file: ctx.sourceFile,
          source_byte_offset: offsetAtLineStart,
        });
        continue;
      }
      // other event_msg payloads (agent_message without token data, errors, …)
      skipped += 1;
      continue;
    }

    skipped += 1;
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
