/**
 * Claude Code JSONL parser.
 *
 * Layout reminder (from ~/.claude/projects/<encoded>/<session>.jsonl):
 *   { type: "user",           message: { role, content }, uuid, timestamp, sessionId, cwd, version, gitBranch, entrypoint }
 *   { type: "assistant",      message: { role, model, id, content, usage: { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens } }, uuid, timestamp, cwd, ... }
 *   { type: "tool_use",       ... }
 *   { type: "attachment",     ... }
 *   { type: "queue-operation", ... }   // noise, skip
 *
 * Known issue: `usage.input_tokens` in this file can be a streaming
 * placeholder. We still record it as-is.
 *
 * Resumed sessions (`claude --resume` / `--continue`): Claude Code writes a
 * NEW <new-session-uuid>.jsonl that BEGINS with byte-identical copies of the
 * ancestor session's lines — each copy keeps its original `sessionId` and
 * `uuid` — followed by the new lines (which carry the new session's id, i.e.
 * the filename's uuid). Without special handling those copies become fresh
 * events (different file+offset → different source_event_id) under the
 * ancestor's session_id, double-counting its tokens after every resume.
 * Policy (see dedupeIdFor below): a line whose sessionId differs from the
 * filename's uuid is a resume copy — skip it when the ancestor's own file
 * still exists (that file is the canonical source), otherwise emit it keyed
 * by line uuid so orphaned history survives exactly once.
 */
import { createHash } from "node:crypto";
import { createReadStream, existsSync, readdirSync } from "node:fs";
import { stat } from "node:fs/promises";
import { dirname, join } from "node:path";
import { createInterface } from "node:readline";
import type { RawEvent } from "@modelstat/core";
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

/** Claude Code's `message.content` is either a plain string (older
 * lines, mainly user messages) or an array of typed content blocks
 * (newer lines + most assistant messages). `text` blocks feed the
 * excerpt; `tool_use` / `tool_result` blocks feed per-call extraction
 * (ToolCallDraft) — their raw input/content is reduced to hashes and
 * byte sizes on the spot and never leaves this module. */
type ClaudeContentBlock =
  | { type: "text"; text?: string }
  | { type: "tool_use"; id?: string; name?: string; input?: unknown }
  | { type: "tool_result"; tool_use_id?: string; is_error?: boolean; content?: unknown }
  | { type: string; [k: string]: unknown };
type ClaudeMessageContent = string | ClaudeContentBlock[] | undefined;

interface ClaudeAssistantLine {
  type: "assistant";
  uuid: string;
  timestamp: string;
  sessionId: string;
  cwd?: string | null;
  gitBranch?: string | null;
  version?: string;
  entrypoint?: string;
  parentUuid?: string | null;
  message: {
    role: "assistant";
    model?: string;
    id?: string;
    content?: ClaudeMessageContent;
    usage?: {
      input_tokens?: number;
      output_tokens?: number;
      cache_creation_input_tokens?: number;
      cache_read_input_tokens?: number;
    };
  };
}

interface ClaudeUserLine {
  type: "user";
  uuid: string;
  timestamp: string;
  sessionId: string;
  cwd?: string | null;
  gitBranch?: string | null;
  parentUuid?: string | null;
  message: {
    role: "user";
    content?: ClaudeMessageContent;
  };
}

type ClaudeLine = ClaudeAssistantLine | ClaudeUserLine | { type: string; [k: string]: unknown };

function decodeEncodedDir(encoded: string): string {
  // Claude encodes / as -. Not a perfect inverse (- in original filenames
  // becomes ambiguous) but good enough for display.
  return encoded.replace(/^-/, "/").replace(/-/g, "/");
}

/**
 * Extract a redacted, ≤320-char text excerpt from a Claude Code
 * message.content. Only `text` blocks contribute; tool_use /
 * tool_result blocks are dropped (their input is structured and not
 * meaningful for a one-sentence summary). Code-fenced blocks are
 * stripped so the summariser focuses on natural language. The result
 * is run through @modelstat/core/redact to scrub secrets / emails /
 * absolute paths before it leaves this function.
 *
 * Returns undefined when there's nothing usable — the schema field is
 * optional, so the pipeline falls back to metadata-only summarisation
 * for those events.
 */
function extractExcerpt(content: ClaudeMessageContent): string | undefined {
  if (!content) return undefined;
  let text = "";
  if (typeof content === "string") {
    text = content;
  } else if (Array.isArray(content)) {
    const parts: string[] = [];
    for (const block of content) {
      if (block && block.type === "text" && typeof block.text === "string") {
        parts.push(block.text);
      }
    }
    text = parts.join(" ");
  }
  if (!text) return undefined;

  // Strip fenced code blocks — they're noise for a summariser that's
  // supposed to describe the work, not the code itself.
  text = text.replace(/```[\s\S]*?```/g, " ").replace(/`[^`]*`/g, " ");
  // Collapse whitespace.
  text = text.replace(/\s+/g, " ").trim();
  if (!text) return undefined;

  // Redact, then trim to the schema cap. The daemon-core pipeline
  // re-redacts as defence-in-depth; this is the first pass.
  const cleaned = redact(text).text;
  const truncated = cleaned.slice(0, 320);
  return truncated.length > 0 ? truncated : undefined;
}

/** Join a message's natural-language TEXT (string content or `text` blocks) at
 * FULL length, for public-reference detection. Unlike {@link extractExcerpt} it
 * does not truncate or strip code fences (a PR URL may sit in backticks or past
 * the 320-char excerpt window). tool_use / tool_result blocks are skipped on
 * purpose — command output and file dumps add example-URL noise and hurt
 * precision. Capped so a giant pasted turn can't make the regex scan unbounded.
 * The text is NOT redacted here: detectEventReferences pulls only public ref
 * shapes from it, never raw text. */
function collectRefText(content: ClaudeMessageContent): string {
  if (!content) return "";
  let text = "";
  if (typeof content === "string") {
    text = content;
  } else if (Array.isArray(content)) {
    const parts: string[] = [];
    for (const block of content) {
      if (block && block.type === "text" && typeof block.text === "string") {
        parts.push(block.text);
      }
    }
    text = parts.join(" ");
  }
  return text.length > 64_000 ? text.slice(0, 64_000) : text;
}

/** Build one ToolCallDraft from an observed tool_use (content block or
 * top-level line form). Starts life unmatched (`status: "unknown"`,
 * `ended_at: null`) — a later tool_result in the same file parse fills
 * those in. PRIVACY: `input` is reduced to sha256 hashes + byte sizes and the
 * on-device extractor's structural `action` (incl. a value-masked shape and a
 * redacted command); the raw value is then discarded. */
function buildToolCallDraft(opts: {
  observedName: string;
  rawCallId: unknown;
  input: unknown;
  sessionId: string;
  sourceEventId: string;
  callIndex: number;
  startedAt: string;
  model: string | null;
  cwd?: string | null;
  /** When provided, the raw command + cwd for this call are pushed here
   * (local-only) so the agent can summarise its referenced script files. */
  contexts?: LocalToolContext[];
}): ToolCallDraft {
  const { server, name } = splitObservedToolName(opts.observedName);
  const hashes = hashArgs(opts.input);
  const external_call_id =
    typeof opts.rawCallId === "string" && opts.rawCallId.trim() !== ""
      ? opts.rawCallId.trim().slice(0, 120)
      : fallbackCallId(opts.sourceEventId, opts.callIndex);
  // Stash the raw command + cwd locally (never shipped) so the Node agent can
  // read + summarise referenced script FILES into ToolAction.scripts. See
  // ParseResult.scriptContexts / LocalToolContext.
  if (opts.contexts) {
    const local = extractLocalToolContext({ server, name, input: opts.input, cwd: opts.cwd });
    if (local) opts.contexts.push({ external_call_id, ...local });
  }
  return {
    external_call_id,
    session_id: opts.sessionId,
    source_event_id: opts.sourceEventId,
    agent: "claude_code",
    server,
    name,
    // This parser never derives a turn index (events above carry
    // turn_index: null too), so per-call records can't either.
    turn_index: null,
    call_index: opts.callIndex,
    started_at: opts.startedAt,
    ended_at: null,
    status: "unknown",
    args_hash: hashes.args_hash,
    signature_hash: hashes.signature_hash,
    args_bytes: hashes.args_bytes,
    result_bytes: 0,
    model: opts.model,
    action: extractToolAction({ server, name, input: opts.input, cwd: opts.cwd }),
  };
}

export async function parseClaudeCodeJsonl(ctx: ParserContext): Promise<ParseResult> {
  // In streaming mode (ctx.onEvents set) `events` stays empty and
  // parsed events leave through the sink in PARSER_EVENT_CHUNK-sized
  // chunks — the parser's working set is bounded no matter how large
  // the transcript is. See ParserContext.onEvents.
  const events: RawEvent[] = [];
  const toolCalls: ToolCallDraft[] = [];
  // Local-only raw command + cwd per shell call, for the agent's script-summary
  // pass. Never shipped (see LocalToolContext); returned on ParseResult.
  const scriptContexts: LocalToolContext[] = [];
  /** tool_use id → draft still waiting for its tool_result (pairing is
   * file-local: anything unmatched at EOF stays status "unknown"). */
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

  const stream = createReadStream(ctx.sourceFile, {
    encoding: "utf8",
    start: startOffset,
  });
  const rl = createInterface({ input: stream, crlfDelay: Infinity });

  // Session-level context captured from the first typed line seen.
  let sessionId: string | null = null;
  let cwd: string | null = null;
  let gitBranch: string | null = null;
  let lastModel: string | null = null;

  // ── Resume-copy dedupe (see module header) ──────────────────────
  // The filename's uuid is the session this file was written FOR; any
  // line carrying a different sessionId is copied ancestor history.
  const filenameSessionId = deriveSessionIdFromFilename(ctx.sourceFile);
  const ancestorExistsCache = new Map<string, boolean>();

  /** Is the ancestor session's own transcript still on disk? Resumes
   * routinely cross project dirs (a worktree per session → a different
   * encoded dir per cwd), so after the fast same-dir probe check every
   * sibling dir under the projects root. Memoised — a resumed file
   * carries only a handful of distinct ancestor ids. */
  function ancestorFileExists(sid: string): boolean {
    const cached = ancestorExistsCache.get(sid);
    if (cached !== undefined) return cached;
    let found = false;
    const dir = dirname(ctx.sourceFile);
    try {
      if (existsSync(join(dir, `${sid}.jsonl`))) {
        found = true;
      } else {
        const root = dirname(dir);
        for (const entry of readdirSync(root, { withFileTypes: true })) {
          if (!entry.isDirectory()) continue;
          if (existsSync(join(root, entry.name, `${sid}.jsonl`))) {
            found = true;
            break;
          }
        }
      }
    } catch {
      // Unreadable root — can't prove the ancestor exists, so fall
      // through to emitting the copy under its uuid key (safe: worst
      // case is one extra, self-deduping record, not lost history).
    }
    ancestorExistsCache.set(sid, found);
    return found;
  }

  /** Dedupe id for the line about to be emitted, or null to drop it.
   * Normal lines keep the historical (file, byteOffset) key — changing
   * that would re-key all previously-ingested history (the server
   * dedupes on source_event_id) and double-count everything once.
   * Resume copies are exactly the subset that double-counts today:
   *   - ancestor file present → null. Its own parse is the canonical
   *     source for these events; emitting the copy too is what inflates
   *     the ancestor session.
   *   - ancestor file gone (e.g. pruned by cleanupPeriodDays) → key by
   *     the line's uuid, which every copy preserves verbatim: the
   *     orphaned history is reported once, and copies of these lines in
   *     later resumes collapse to the same id server-side. */
  function dedupeIdFor(lineUuid: string, byteOffset: number): string | null {
    const isResumeCopy = filenameSessionId !== null && sessionId !== filenameSessionId;
    if (!isResumeCopy) return sourceEventId(ctx.deviceId, ctx.sourceFile, byteOffset);
    if (ancestorFileExists(sessionId!)) return null;
    return sourceEventId(ctx.deviceId, { lineUuid });
  }

  for await (const line of rl) {
    const byteLen = Buffer.byteLength(line, "utf8") + 1; // +1 for the stripped \n
    const offsetAtLineStart = startOffset + bytePos;
    bytePos += byteLen;
    rawLines += 1;
    if (!line.trim()) {
      skipped += 1;
      continue;
    }

    let obj: ClaudeLine;
    try {
      obj = JSON.parse(line) as ClaudeLine;
    } catch {
      skipped += 1;
      continue;
    }

    if (obj.type === "queue-operation") {
      skipped += 1;
      continue;
    }

    if (obj.type === "user" || obj.type === "assistant") {
      const u = obj as ClaudeUserLine | ClaudeAssistantLine;
      sessionId = u.sessionId ?? sessionId;
      cwd = u.cwd ?? cwd;
      gitBranch = u.gitBranch ?? gitBranch;
    }

    if (obj.type === "assistant") {
      const a = obj as ClaudeAssistantLine;
      const usage = a.message?.usage ?? {};
      // Claude Code records locally-generated messages (error notices,
      // "No response requested.") with model "<synthetic>". The event
      // itself keeps that model verbatim, but it must not bleed into
      // `lastModel` — the user messages that follow belong to the real
      // model the session is running on.
      if (a.message?.model && a.message.model !== "<synthetic>") {
        lastModel = a.message.model;
      }
      if (!a.uuid || !sessionId) {
        skipped += 1;
        continue;
      }
      // Resume-copy handling comes after the context capture above so
      // lastModel/cwd still flow from copied history into the new lines.
      const eventId = dedupeIdFor(a.uuid, offsetAtLineStart);
      if (eventId === null) {
        skipped += 1;
        continue;
      }

      const slug = guessRepoSlugFromPath(cwd);
      const excerpt = extractExcerpt(a.message?.content);
      const refs = detectEventReferences(collectRefText(a.message?.content));

      // Per-call tool_use extraction + the aggregate identity → count
      // map carried by this assistant event. Drafts are returned via
      // ParseResult.toolCalls — they never become RawEvents (the
      // pipeline would treat those as turns).
      const aggregate: Record<string, number> = {};
      const blocks = Array.isArray(a.message?.content) ? a.message.content : [];
      let callIndex = 0;
      for (const block of blocks) {
        if (!block || block.type !== "tool_use") continue;
        const index = callIndex;
        callIndex += 1;
        const observed = typeof block.name === "string" ? block.name.trim() : "";
        if (!observed) continue;
        const draft = buildToolCallDraft({
          observedName: observed,
          rawCallId: block.id,
          input: block.input,
          sessionId,
          sourceEventId: eventId,
          callIndex: index,
          startedAt: a.timestamp,
          // Model verbatim from the issuing assistant message —
          // including "<synthetic>" (same rule as the event below).
          model: a.message?.model ?? null,
          cwd,
          contexts: scriptContexts,
        });
        const identity = toolIdentity(draft.server, draft.name);
        aggregate[identity] = (aggregate[identity] ?? 0) + 1;
        toolCalls.push(draft);
        if (typeof block.id === "string" && block.id) pendingByCallId.set(block.id, draft);
      }

      await emit({
        source_event_id: eventId,
        ts: a.timestamp,
        kind: "assistant_message",
        agent: "claude_code",
        provider: "anthropic",
        model: a.message?.model ?? null,
        session_id: sessionId,
        turn_index: null,
        parent_event_id: a.parentUuid ?? null,
        cwd,
        // Emit git context whenever there is ANY signal — a repo slug OR the
        // session's branch. Gating on `slug` alone dropped the branch (and all
        // git context) for cwds the path heuristic doesn't match (e.g.
        // ~/Documents/<repo>), starving the session-metadata join. The branch
        // is the historical one Claude Code recorded for the turn.
        git:
          slug || gitBranch
            ? {
                remote_url: null,
                remote_host: slug?.includes("/") ? "github.com" : null,
                remote_slug: slug,
                branch: gitBranch,
                commit_sha: null,
              }
            : null,
        tokens: {
          input: usage.input_tokens ?? 0,
          output: usage.output_tokens ?? 0,
          cache_creation: usage.cache_creation_input_tokens ?? 0,
          cache_read: usage.cache_read_input_tokens ?? 0,
          reasoning: 0,
        },
        duration_ms: null,
        tool_calls: aggregate,
        files_touched: [],
        ...(excerpt ? { content_excerpt: excerpt } : {}),
        ...(refs ? { references: refs } : {}),
        source_file: ctx.sourceFile,
        source_byte_offset: offsetAtLineStart,
        // Files in ~/.claude/projects/ come from the Claude Code app
        // used via subscription (not the raw API). Mark them so the
        // server short-circuits token-level cost to $0 — the user has
        // already paid the flat monthly fee.
        pricing_mode: "subscription",
      });
    } else if (obj.type === "user") {
      const u = obj as ClaudeUserLine;

      // Pair tool_result blocks back to their pending tool_use drafts
      // (matched by tool_use_id). Done before the emission guard below —
      // a result line that can't emit a user event still ends the call.
      // Only the result's hash-safe metadata is kept: timestamp, error
      // flag, and the byte size of its content.
      const uContent = u.message?.content;
      if (Array.isArray(uContent)) {
        for (const block of uContent) {
          if (!block || block.type !== "tool_result") continue;
          const ref = block.tool_use_id;
          if (typeof ref !== "string") continue;
          const draft = pendingByCallId.get(ref);
          if (!draft) continue;
          pendingByCallId.delete(ref);
          draft.ended_at = u.timestamp;
          draft.status = block.is_error === true ? "error" : "success";
          draft.result_bytes = jsonBytes(block.content);
        }
      }

      if (!u.uuid || !sessionId) {
        skipped += 1;
        continue;
      }
      const eventId = dedupeIdFor(u.uuid, offsetAtLineStart);
      if (eventId === null) {
        skipped += 1;
        continue;
      }
      const excerpt = extractExcerpt(u.message?.content);
      const refs = detectEventReferences(collectRefText(u.message?.content));
      await emit({
        source_event_id: eventId,
        ts: u.timestamp,
        kind: "user_message",
        agent: "claude_code",
        provider: "anthropic",
        model: lastModel,
        session_id: sessionId,
        turn_index: null,
        parent_event_id: u.parentUuid ?? null,
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
        pricing_mode: "subscription",
      });
    } else if (obj.type === "tool_use") {
      // Top-level `type:'tool_use'` line form (see the header comment) —
      // the same {id, name, input} shape as the content-block variant,
      // standalone on its own line. It yields a per-call draft only and
      // must NOT become a RawEvent (the pipeline would count it as a
      // turn). With no containing assistant event, the draft anchors on
      // a source_event_id derived from this line's own byte offset —
      // mirroring how codex response_item lines anchor theirs.
      const t = obj as {
        type: "tool_use";
        id?: unknown;
        name?: unknown;
        input?: unknown;
        timestamp?: unknown;
        sessionId?: unknown;
      };
      const observed = typeof t.name === "string" ? t.name.trim() : "";
      const ts = typeof t.timestamp === "string" ? t.timestamp : null;
      const sid = (typeof t.sessionId === "string" ? t.sessionId : null) ?? sessionId;
      if (!observed || !ts || !sid) {
        skipped += 1;
        continue;
      }
      const draft = buildToolCallDraft({
        observedName: observed,
        rawCallId: t.id,
        input: t.input,
        sessionId: sid,
        sourceEventId: sourceEventId(ctx.deviceId, ctx.sourceFile, offsetAtLineStart),
        callIndex: 0,
        startedAt: ts,
        // No issuing assistant message on this line — attribute to the
        // session's last real model, same as user_message attribution
        // (lastModel never holds "<synthetic>", per the rule above).
        model: lastModel,
        cwd,
        contexts: scriptContexts,
      });
      toolCalls.push(draft);
      if (typeof t.id === "string" && t.id) pendingByCallId.set(t.id, draft);
    } else {
      skipped += 1;
    }
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

export function deriveSessionIdFromFilename(path: string): string | null {
  const m = /([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$/.exec(path);
  return m ? (m[1] ?? null) : null;
}

export { decodeEncodedDir };

/** Quick checksum of a JSONL file (size + last-line tail) used by the
 * discovery layer to decide if a re-parse is needed. */
export async function quickChecksum(
  path: string,
): Promise<{ size: number; mtime: number; tailHash: string }> {
  const st = await stat(path);
  const stream = createReadStream(path, {
    start: Math.max(0, st.size - 4096),
    encoding: "utf8",
  });
  const h = createHash("sha1");
  for await (const chunk of stream) h.update(chunk as string);
  return { size: st.size, mtime: st.mtimeMs, tailHash: h.digest("hex").slice(0, 16) };
}
