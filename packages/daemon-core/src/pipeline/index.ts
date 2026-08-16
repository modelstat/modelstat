/**
 * Daemon pipeline — redact → tokenize → segment → summarise → tag.
 *
 * The runtime (CLI on Node via Ollama, extension in browser via WebLLM)
 * passes in concrete adapters; this module encodes the algorithm once
 * and leaves the heavy ML calls to the adapter layer.
 *
 * Segmentation strategy
 *   - Time gap ≥ SEGMENT_TIME_GAP_MS (15 min) → new segment
 *   - Cosine distance between consecutive turn embeddings >
 *     SEGMENT_TOPIC_THRESHOLD (0.35) → new segment
 *   - Hard cap: SEGMENT_MAX_TURNS (100) or SEGMENT_MAX_DURATION_MS
 *     (30 min) — whichever first
 *   - Singletons merged into neighbours so every emitted segment has
 *     ≥ 2 turns (unless the whole session is one turn).
 */

import type { Agent } from "@modelstat/core/enums";
import { segmentId } from "@modelstat/core/ids";
import { redact } from "@modelstat/core/redact";
import {
  PROJECT_SLUG_CONFIDENCE_GUESS,
  PROJECT_SLUG_CONFIDENCE_VERIFIED,
  type RawEvent,
  type Segment,
  slugIsVerified,
} from "@modelstat/core/schemas";
import {
  type CognitionTags,
  cognitionHints,
  type Cognizer,
  formatCognitionSuffix,
} from "./cognition.js";
import { ABSTRACT_OUTPUT_MAX_CHARS, SUMMARISER_MAX_TOKENS } from "./prompts.js";
import type { LinkExtractor } from "./session-metadata.js";
import type { Entitler } from "./title.js";

// ── Adapter types ────────────────────────────────────────────────

export type Embedder = (text: string) => Promise<number[]>;
export type Summarizer = (input: {
  prompt: string;
  maxTokens: number;
  /** Structured excerpts (the sampled, redacted conversation turns) the `prompt`
   * was built from. The LLM path uses `prompt`; the dependency-free heuristic
   * fallback uses these directly. Optional for back-compat with other adapters. */
  excerpts?: string[];
  /** One-line structural facts (repo, turns, files, tools) — same source as the
   * prompt's `Session context:` line. Used by the heuristic fallback. */
  facts?: string;
}) => Promise<string>;

export { heuristicSummarize } from "./heuristic-summary.js";
export type Tokenizer = (text: string) => number | Promise<number>;
/**
 * Optional model-based redactor that runs AFTER the regex pass.
 * Typical implementation: OpenAI Privacy Filter via Transformers.js
 * — see `createPrivacyFilterRedactor` in
 * @modelstat/daemon-core/redact/privacy-filter. Returns the same
 * shape the regex pass returns so the pipeline can compose them
 * without special-casing.
 */
export type Redactor = (text: string) => Promise<{
  text: string;
  counts: Record<string, number>;
}>;

export interface PipelineAdapters {
  embed: Embedder;
  summarize: Summarizer;
  tokenize: Tokenizer;
  /** Optional second-pass redactor. When omitted, only the regex pass
   * runs (back-compatible with consumers who haven't added the
   * Privacy Filter adapter yet). */
  redact?: Redactor;
  /** Optional cognition pass — runs after summarise + redact, tags the
   * user's emotional state and meta-cognitive mode, and APPENDS the
   * result to the abstract as a human-readable suffix
   * (`[Mood: frustrated, curious] [Mind: debugging]`). Nothing else
   * about the segment changes — the tags travel inside the abstract
   * text (no hardcoded enum, no dedicated wire field).
   *
   * Best-effort: runtime failures or null replies leave the abstract
   * unchanged. Cognition is a fun / auxiliary feature, NOT part of
   * the core pipeline contract. */
  cognize?: Cognizer;
  /** Optional session-title pass — used by `buildSessionTitles` (NOT
   * by the per-segment flow) to name each session from its segment
   * abstracts for the dashboard's sessions list. Best-effort: when
   * absent or failing, titles fall back to a deterministic derivation
   * from the first abstract. See pipeline/title.ts. */
  entitle?: Entitler;
  /** Optional on-device link-extraction pass — used by
   * `buildSessionMetadata` (NOT by the per-segment flow) to surface
   * PR/issue/commit references from a session's redacted abstracts for ANY
   * provider. Best-effort: when absent or failing, deterministic git +
   * content detection still runs. See pipeline/session-metadata.ts. */
  extractLinks?: LinkExtractor;
}

/**
 * Per-segment progress, emitted by {@link buildSegmentsForSession} as it
 * works through a batch so a long summarise pass isn't an opaque block to
 * the UI. The Node daemon turns these into the live "Summarising 3/8 ·
 * session 2/4" line the menu-bar tray shows; the browser path can ignore
 * it.
 *
 * `segment === 0` means the session is still being analyzed/segmented
 * (turn embeddings + boundary detection) so the per-session slice total
 * isn't known yet — render it as "Analyzing…".
 */
export interface SegmentProgress {
  /** 1-based index of the session being processed within this batch. */
  session: number;
  /** Total number of sessions in this batch. */
  sessionTotal: number;
  /** 1-based index of the segment within the current session, or 0 while
   * the session is still being segmented (totals not yet known). */
  segment: number;
  /** Number of segments in the current session, or 0 if not yet known. */
  segmentTotal: number;
}
export type SegmentProgressFn = (p: SegmentProgress) => void;

// ── Constants ────────────────────────────────────────────────────

/** Start a new segment when two consecutive turns are more than 15 min apart. */
export const SEGMENT_TIME_GAP_MS = 15 * 60_000;

/** Start a new segment when the cosine distance between two adjacent
 * turn embeddings exceeds this threshold. Tuned on dogfood data; raise
 * to split more aggressively, lower to keep sessions together. */
export const SEGMENT_TOPIC_THRESHOLD = 0.35;

export const SEGMENT_MAX_TURNS = 100;
export const SEGMENT_MAX_DURATION_MS = 30 * 60_000;

/**
 * Hard char-count ceiling on a segment's accumulated `content_excerpt`
 * length, summed across turns. Beyond this the bundled summariser
 * starts losing focus and produces vague "many things were done"
 * sentences instead of one precise tweet.
 *
 * Empirically tuned for Qwen3.5-4B (the bundled model): with our
 * sampling strategy of 7 excerpts × 350 chars = ~2.5K chars of
 * actual conversation content sent to the model, segments larger
 * than ~12K chars start sampling so sparsely that the abstract
 * loses precision. Below ~3K chars the model has so little to
 * summarise it produces generic genre descriptions. Sweet spot
 * is roughly 4K-12K chars per segment.
 *
 * The cap fires AFTER the time-gap, max-turns, and topic-shift
 * checks, so it only kicks in for long, on-topic, fast-typing
 * sessions where none of the natural boundaries fired.
 */
export const SEGMENT_MAX_CONTENT_CHARS = 12_000;

/** Storage cap on the `abstract` column. Larger than the user-visible
 * output cap (`ABSTRACT_OUTPUT_MAX_CHARS` = 200) so a future
 * longer-output prompt still fits in the schema without a migration. */
export const ABSTRACT_MAX_CHARS = 512;

/**
 * Excerpt sampling for the summariser prompt. Tuned for Qwen3.5-4B:
 *   - 7 excerpts spans the segment evenly enough that the model can
 *     describe the arc (start → resolution) instead of just the first
 *     turn's intent.
 *   - 350 chars per excerpt keeps the total input under 2.5 KB,
 *     leaving the model plenty of context window for thinking + output
 *     while still capturing enough substance per turn that small models
 *     can latch onto specific details.
 *   - Combined budget (~2.5K chars input → ~700 input tokens) matches
 *     the empirical sweet spot for instruction-following on small
 *     summarisation tasks; pushing higher dilutes attention.
 */
export const SUMMARISER_EXCERPT_COUNT = 7;
export const SUMMARISER_EXCERPT_MAX_CHARS = 350;

// ── Public API ───────────────────────────────────────────────────

/**
 * Build one-or-more Segments from a list of same-session RawEvents.
 * Given the adapter set, runs redact → segment boundaries → per-
 * segment summarise → per-segment tag. Emits deterministic tags
 * (project / provider / model / tool / environment) from event
 * metadata; LLM-derived tags (work_type, domain, components) are
 * attached per segment.
 */
export async function buildSegmentsForSession(
  events: RawEvent[],
  adapters: PipelineAdapters,
  onProgress?: SegmentProgressFn,
): Promise<Segment[]> {
  if (events.length === 0) return [];
  // Group by session_id — defensive; caller should do this already.
  const bySession = new Map<string, RawEvent[]>();
  for (const ev of events) {
    const arr = bySession.get(ev.session_id) ?? [];
    arr.push(ev);
    bySession.set(ev.session_id, arr);
  }
  const sessions = [...bySession.entries()];
  const out: Segment[] = [];
  for (let si = 0; si < sessions.length; si++) {
    const [sessionId, evs] = sessions[si]!;
    const segs = await buildForOneSession(
      sessionId,
      evs,
      adapters,
      onProgress
        ? (segment, segmentTotal) =>
            onProgress({ session: si + 1, sessionTotal: sessions.length, segment, segmentTotal })
        : undefined,
    );
    out.push(...segs);
  }
  return out;
}

async function buildForOneSession(
  sessionId: string,
  events: RawEvent[],
  adapters: PipelineAdapters,
  onSlice?: (segment: number, segmentTotal: number) => void,
): Promise<Segment[]> {
  // Announce the session immediately. The embedding + boundary-detection
  // work below can take several seconds on a large session, and without
  // this the UI would sit frozen on the previous line until the first
  // slice is summarised. `0` total = "still analyzing, count unknown".
  onSlice?.(0, 0);
  const sorted = [...events].sort((a, b) => a.ts.localeCompare(b.ts));

  // Pre-compute turn embeddings from a stable surface: kind + model +
  // tool-call summary. Content would leak PII; these tags are metadata
  // and already redacted.
  //
  // Embed failures are tolerated turn-by-turn: one slow/missing
  // embedder should NOT kill the scan cycle. If embeddings are not
  // available, segment boundaries fall back to time-gap / max-turns
  // heuristics, which are good enough for v1. This was the reason every
  // CLI upload had been dying silently — a missing Ollama on the user's
  // machine would throw out of this loop, break `buildSegments`, and
  // propagate up to "Upload failed: fetch failed" without ever actually
  // hitting /v1/ingest.
  const turnSurfaces = sorted.map((e) => turnSurface(e));
  const turnEmbeddings: number[][] = [];
  for (const s of turnSurfaces) {
    if (s.length === 0) {
      turnEmbeddings.push([]);
      continue;
    }
    try {
      turnEmbeddings.push(await adapters.embed(s));
    } catch {
      turnEmbeddings.push([]);
    }
  }

  // Compute boundary indices (exclusive end of each slice).
  // Track running content size so we can split LONG-on-topic
  // sessions before they exceed the bundled summariser's
  // attention sweet spot (see SEGMENT_MAX_CONTENT_CHARS docs).
  const boundaries: number[] = [];
  let runStart = 0;
  let runStartMs = Date.parse(sorted[0]!.ts);
  let runChars = (sorted[0]!.content_excerpt ?? "").length;
  for (let i = 1; i < sorted.length; i++) {
    const prev = sorted[i - 1]!;
    const cur = sorted[i]!;
    const gap = Date.parse(cur.ts) - Date.parse(prev.ts);
    const runMs = Date.parse(cur.ts) - runStartMs;
    const turnsInRun = i - runStart;
    let split = false;
    if (gap >= SEGMENT_TIME_GAP_MS) split = true;
    else if (turnsInRun >= SEGMENT_MAX_TURNS) split = true;
    else if (runMs >= SEGMENT_MAX_DURATION_MS) split = true;
    else if (runChars >= SEGMENT_MAX_CONTENT_CHARS) split = true;
    else if (
      turnEmbeddings[i - 1]!.length > 0 &&
      turnEmbeddings[i]!.length > 0 &&
      cosineDistance(turnEmbeddings[i - 1]!, turnEmbeddings[i]!) > SEGMENT_TOPIC_THRESHOLD
    ) {
      split = true;
    }
    if (split) {
      boundaries.push(i);
      runStart = i;
      runStartMs = Date.parse(cur.ts);
      runChars = (cur.content_excerpt ?? "").length;
    } else {
      runChars += (cur.content_excerpt ?? "").length;
    }
  }
  boundaries.push(sorted.length);

  // Materialise slices.
  const slices: RawEvent[][] = [];
  let prev = 0;
  for (const b of boundaries) {
    slices.push(sorted.slice(prev, b));
    prev = b;
  }

  // Merge tiny singletons into neighbour to avoid per-turn fragments.
  const merged: RawEvent[][] = [];
  for (const slice of slices) {
    const last = merged[merged.length - 1];
    if (slice.length === 1 && last) {
      last.push(...slice);
    } else {
      merged.push(slice);
    }
  }

  const segments: Segment[] = [];
  let failed = 0;
  let lastError: string | null = null;
  for (let mi = 0; mi < merged.length; mi++) {
    const slice = merged[mi]!;
    // Report progress before each slice so the daemon's heartbeat (and
    // the tray reading it) advances on every summarise step — the long
    // pole during a backfill — instead of looking wedged for minutes.
    onSlice?.(mi + 1, merged.length);
    try {
      const seg = await summariseSlice(sessionId, slice, adapters);
      if (seg) segments.push(seg);
    } catch (err) {
      // Per-slice failure (no excerpts, model error, model timeout)
      // must NOT block the rest of the batch. The previous behaviour
      // — propagating the throw — meant a single all-tool_use slice
      // killed the whole upload, leaving thousands of healthy events
      // queued forever and the daemon wedged on "Uploading X events"
      // with zero /v1/ingest hits to show for it. Track the failures
      // for visibility but keep summarising the rest.
      failed += 1;
      lastError = err instanceof Error ? err.message : String(err);
      // biome-ignore lint/suspicious/noConsole: per-slice failure
      console.warn(`[modelstat] slice failed in session ${sessionId}: ${lastError}`);
    }
  }
  if (failed > 0) {
    // biome-ignore lint/suspicious/noConsole: end-of-batch summary
    console.warn(
      `[modelstat] session ${sessionId}: ${segments.length} segments built, ${failed} slices failed (last error: ${lastError ?? "?"}). Continuing with the healthy slices.`,
    );
  }
  return segments;
}

async function summariseSlice(
  sessionId: string,
  slice: RawEvent[],
  adapters: PipelineAdapters,
): Promise<Segment | null> {
  if (slice.length === 0) return null;
  const first = slice[0]!;
  const last = slice[slice.length - 1]!;
  const startedAtMs = Date.parse(first.ts);
  const endedAtMs = Date.parse(last.ts);

  // Token totals from upstream parser — exact for JSONL, tokenizer
  // fallback for browser captures.
  const tokens = {
    input: 0,
    output: 0,
    cache_creation: 0,
    cache_read: 0,
    reasoning: 0,
  };
  for (const ev of slice) {
    if (!ev.tokens) continue;
    tokens.input += ev.tokens.input;
    tokens.output += ev.tokens.output;
    tokens.cache_creation += ev.tokens.cache_creation;
    tokens.cache_read += ev.tokens.cache_read;
    tokens.reasoning += ev.tokens.reasoning;
  }

  // Build the summariser input from BOTH structural facts and
  // sampled conversation excerpts. Parsers populate `content_excerpt`
  // per RawEvent (after running PII redaction on the raw turn text).
  // Without these excerpts the summariser has nothing but metadata to
  // work with — the resulting abstract collapses to "100 turns on
  // claude_code", a placeholder that downstream consumers reject.
  const promptFacts = [
    first.git?.remote_slug ? `repo ${first.git.remote_slug}` : null,
    first.git?.branch ? `branch ${first.git.branch}` : null,
    `${slice.length} turns on ${first.agent}`,
    first.files_touched?.length
      ? `files touched: ${first.files_touched.slice(0, 5).join(", ")}`
      : null,
    Object.keys(first.tool_calls ?? {}).length
      ? `tool calls: ${Object.keys(first.tool_calls).slice(0, 5).join(", ")}`
      : null,
  ]
    .filter(Boolean)
    .join("; ");

  // Sample excerpts: first turn (usually the user's intent), middle,
  // last (usually resolution). Plus quartile picks. Re-redact on our
  // side as defence-in-depth even though the parser is supposed to
  // have already redacted.
  const excerpts = sampleAndRedactExcerpts(slice);
  if (excerpts.length === 0) {
    // No usable content excerpts in this slice — usually a parser
    // bug (extractExcerpt stripped everything as code blocks) or a
    // session that was 100% tool_use with no prose. Refuse to
    // summarise without content; the alternative is feeding the
    // model a metadata-only prompt and getting "100 turns on
    // claude_code" back, which downstream then refuses to classify
    // (PlaceholderAbstractError). Fail at the SOURCE so the user's
    // logs say exactly what's wrong instead of failing later, far from
    // the cause.
    throw new Error(
      `parser produced 0 content excerpts for session ${sessionId} (${slice.length} turns) — the summariser would only see metadata and produce "${slice.length} turns on ${first.agent}". Check the parser for ${first.agent} (likely extractExcerpt stripped everything as code or the session is pure tool_use).`,
    );
  }
  const excerptBlock = excerpts
    .map((e, i) => `  [turn ${i + 1}] "${e.replace(/\s+/g, " ").trim()}"`)
    .join("\n");

  const prompt = `Session context: ${promptFacts || "generic coding session"}.

Sampled excerpts from the conversation (already redacted of PII and secrets):
${excerptBlock}

Write a ≤${ABSTRACT_OUTPUT_MAX_CHARS}-char summary (1-2 sentences) naming exactly what was achieved: the concrete action, what it acted on, and the specific target (repo/branch/service/component) when identifiable from the context above. Lead with an outcome verb and pack in concrete domain keywords (frameworks, features, decisions). Skip narration and filler.`;

  // Summarisation is core product output, not an optional polish
  // step. If the adapter throws, propagate — silently writing the
  // metadata template ("100 turns on claude_code") as the abstract
  // makes the dashboard look fine while every segment is useless.
  // Empty output is treated as a failure for the same reason. The
  // caller (scan loop) is responsible for surfacing the error and
  // not advancing the queue cursor on failed batches.
  const rawAbstract = await adapters.summarize({
    prompt,
    maxTokens: SUMMARISER_MAX_TOKENS,
    // Structured inputs for the dependency-free fallback summariser (used when
    // the bundled LLM can't load); the LLM path ignores these and uses `prompt`.
    excerpts,
    facts: promptFacts,
  });
  if (!rawAbstract || rawAbstract.trim().length === 0) {
    throw new Error(
      `summariser returned empty abstract for session ${sessionId} (${slice.length} turns) — check that the configured summariser is healthy`,
    );
  }

  // Two-pass redaction: regex always (cheap, microseconds), then
  // optional model-based pass (Privacy Filter via the redact adapter).
  // Failures in the model pass are swallowed — the regex result is
  // shipped as-is so a transient issue never blocks ingest.
  const regexPass = redact(rawAbstract);
  let abstractText = regexPass.text;
  const counts: Record<string, number> = { ...regexPass.counts };
  if (adapters.redact) {
    try {
      const modelPass = await adapters.redact(regexPass.text);
      abstractText = modelPass.text;
      for (const [k, v] of Object.entries(modelPass.counts)) {
        if (k.startsWith("pf_")) counts[k] = v;
      }
    } catch {
      // Keep regex result; server-side defence-in-depth picks up the slack.
    }
  }
  const redacted = { text: abstractText, counts };

  // Cognition pass — best-effort. Reads the redacted abstract and
  // returns short mood / meta-cognitive tags, which we APPEND as a
  // human-readable suffix. The tags travel inside the abstract text
  // (no special primitives, no schema columns, no wire-format
  // additions). If the runtime is unavailable or returns null the
  // abstract is unchanged.
  let cognition: CognitionTags | null = null;
  if (adapters.cognize) {
    try {
      cognition = await adapters.cognize({ abstract: redacted.text });
    } catch {
      /* cognition pass is best-effort; keep the bare abstract */
    }
  }
  const cognitionSuffix = cognition ? formatCognitionSuffix(cognition) : "";
  const abstractWithCognition = cognitionSuffix
    ? `${redacted.text} ${cognitionSuffix}`
    : redacted.text;

  // Privacy-preserving behavioral signal — COUNTS/RATIOS ONLY, never raw
  // text (mirrors RedactionReport). Powers server-side prompt-friction
  // detection: how many user turns / corrections happened plus a 0-1
  // frustration estimate. Computed on-device from event structure +
  // cognition mood tags; nothing identifiable leaves the machine.
  const behavior = computeBehavior(slice, cognition);

  // Deterministic tags from event metadata — no LLM needed. These always apply.
  const tags: Segment["tags"] = [
    { root_key: "agents", name: first.agent, confidence: 1 },
    { root_key: "providers", name: first.provider, confidence: 1 },
  ];
  if (first.model) tags.push({ root_key: "models", name: first.model, confidence: 1 });
  // The projects hint reads the first event whose slug is VERIFIED
  // (`slugIsVerified`), falling back to the first event with any slug — a
  // slice can open on a guessed context (cwd outside the repo) and reach the
  // real one a turn later. Confidence states the provenance tier so the server
  // can gate project-node minting on verified identity; `reason` carries the
  // event's `slug_source` verbatim so the server reads the exact tier.
  const projectGit = (
    slice.find((e) => e.git?.remote_slug && slugIsVerified(e.git)) ??
    slice.find((e) => e.git?.remote_slug)
  )?.git;
  if (projectGit?.remote_slug) {
    tags.push({
      root_key: "projects",
      name: projectGit.remote_slug,
      confidence: slugIsVerified(projectGit)
        ? PROJECT_SLUG_CONFIDENCE_VERIFIED
        : PROJECT_SLUG_CONFIDENCE_GUESS,
      ...(projectGit.slug_source != null ? { reason: projectGit.slug_source } : {}),
    });
  }
  if (first.git?.branch) {
    const env = inferEnvironment(first.git.branch);
    if (env) tags.push({ root_key: "environments", name: env, confidence: 0.7 });
  }

  // Components hint from files_touched — keep the unique top-level paths.
  const components = new Set<string>();
  for (const ev of slice) {
    for (const f of ev.files_touched ?? []) {
      const seg = f.split("/").slice(0, 2).join("/");
      if (seg) components.add(seg);
    }
  }
  for (const c of [...components].slice(0, 8)) {
    tags.push({ root_key: "components", name: c, confidence: 0.6 });
  }

  // Local-time dimensions (WHEN the work happened). The daemon runs on the
  // engineer's machine, so it has their wall-clock; the server only ever sees
  // UTC and could never derive these. The taxonomy temporal/cadence drivers turn
  // them into Time-of-Day / Cadence nodes — only for buckets that actually occur.
  tags.push(...temporalHints(startedAtMs));

  // Mood + Posture dimensions (the HUMAN behind the work) from the best-effort
  // cognition pass — the PRIMARY emotion + stance become one leaf each via the
  // server's Mood/Posture drivers (the full set stays in the abstract suffix). A
  // no-op when cognition was unavailable or empty, so the dimensions are
  // real-data-only by construction.
  tags.push(...cognitionHints(cognition));

  // Tool-call mix — aggregate the per-event count maps (canonical
  // identity → calls, see RawEvent.tool_calls) across the slice and
  // tag the top-8 identities. Confidence encodes share-of-calls
  // (rounded to 2dp) so heavy hitters rank higher at classify time,
  // floored at 0.05 so a single call in a busy slice still registers.
  // 8 tool tags + the ≤13 tags above stay well under the 40-tag cap
  // on Segment.tags.
  const toolCallCounts = new Map<string, number>();
  let toolCallTotal = 0;
  for (const ev of slice) {
    for (const [identity, n] of Object.entries(ev.tool_calls ?? {})) {
      if (!(n > 0)) continue;
      toolCallCounts.set(identity, (toolCallCounts.get(identity) ?? 0) + n);
      toolCallTotal += n;
    }
  }
  const topToolCalls = [...toolCallCounts.entries()]
    // TaxonomyHintRooted.name caps at 120 chars; a pathological MCP
    // identity (`mcp:<server≤116>/<tool≤120>`) can exceed it and would
    // 400 the whole batch server-side. Such identities can't ride a
    // hint at all (truncating would mismatch the server-side leaf), so
    // skip them here — the per-call ToolCallWire rows still carry them
    // via the separately-capped server/name fields.
    .filter(([identity]) => identity.length <= 120)
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .slice(0, 8);
  for (const [identity, count] of topToolCalls) {
    const share = Math.round((count / toolCallTotal) * 100) / 100;
    tags.push({
      root_key: "tool_calls",
      name: identity,
      confidence: Math.min(1, Math.max(0.05, share)),
    });
  }

  // Mean embedding across turn embeddings as the segment embedding —
  // cheaper than re-embedding the (short) abstract and keeps the
  // vector grounded in the actual content.
  let segmentEmbedding: number[] | undefined;
  try {
    const embedded = await adapters.embed(redacted.text.slice(0, ABSTRACT_MAX_CHARS));
    if (embedded.length > 0) segmentEmbedding = embedded;
  } catch {
    segmentEmbedding = undefined;
  }

  // User-intent distillation — from the DEVELOPER'S MESSAGES ONLY (not the
  // assistant's actions or tool calls). This is the source Insights' rule +
  // skill detectors mine: "what does the user actually ask for / how do they
  // direct the AI" — which the outcome abstract (what the assistant DID) can't
  // answer. On-device, redacted, bounded, best-effort.
  const userIntent = await summariseUserIntent(slice, adapters);

  const sourceEventIds = slice.map((e) => e.source_event_id);
  const id = segmentId(sessionId, startedAtMs, endedAtMs, sourceEventIds);
  return {
    segment_id: id,
    session_id: sessionId,
    agent: first.agent as Agent,
    started_at: first.ts,
    ended_at: last.ts,
    // Slice to the user-visible cap (ABSTRACT_OUTPUT_MAX_CHARS, default
    // 400) — well below the 512 storage cap. Models occasionally
    // overshoot the prompt's "≤N chars" instruction; the slice is the
    // hard guarantee the dashboard relies on.
    abstract: abstractWithCognition.slice(0, ABSTRACT_OUTPUT_MAX_CHARS),
    tokens,
    tags,
    // counts is `Record<string, number>` after the optional model
    // merge; the schema's RedactionReport requires the three regex
    // counters (always populated from regexPass.counts) plus a
    // number-valued catchall for pf_*.
    redaction: redacted.counts as Segment["redaction"],
    source_event_ids: sourceEventIds,
    abstract_embedding:
      segmentEmbedding && segmentEmbedding.length === 384 ? segmentEmbedding : undefined,
    behavior,
    user_intent: userIntent,
  };
}

/** Caps for the user-intent distillation (separate from the abstract so the
 * abstract's contract — length, prompt, fallback — is left untouched). */
const USER_INTENT_MAX_CHARS = 240;
const USER_INTENT_MAX_TOKENS = 120;

/** Distill what the DEVELOPER asked for / how they directed the AI, from their
 * OWN messages only — the source Insights' rule + skill detectors mine. The
 * outcome abstract describes what the assistant DID, which is the wrong signal
 * for "what does the user want". On-device, redacted, bounded, best-effort:
 * returns undefined when there's no user prose or the summariser is unavailable. */
async function summariseUserIntent(
  slice: RawEvent[],
  adapters: PipelineAdapters,
): Promise<string | undefined> {
  const userExcerpts = slice
    .filter((e) => e.kind === "user_message")
    .map((e) => e.content_excerpt?.replace(/\s+/g, " ").trim())
    .filter((x): x is string => !!x && x.length > 0);
  if (userExcerpts.length === 0) return undefined;
  // The ask is usually first; later messages add direction/corrections.
  const sample =
    userExcerpts.length <= 6
      ? userExcerpts
      : [...userExcerpts.slice(0, 4), ...userExcerpts.slice(-2)];
  const block = sample.map((e, i) => `  [msg ${i + 1}] "${e.slice(0, 240)}"`).join("\n");
  try {
    const raw = await adapters.summarize({
      prompt: `The developer's own messages to an AI coding assistant (already redacted of PII and secrets):
${block}

In ≤${USER_INTENT_MAX_CHARS} chars, summarise WHAT THE DEVELOPER ASKED FOR or DIRECTED — their goal or task in their own framing, AND any standing preferences / directives / conventions they expressed (e.g. "always be thorough", "ship fast", a naming or workflow convention). Focus on the DEVELOPER'S intent and voice, NOT what the assistant did. Reply with only the summary.`,
      maxTokens: USER_INTENT_MAX_TOKENS,
      excerpts: sample,
      facts: "",
    });
    if (!raw || !raw.trim()) return undefined;
    // Same two-pass redaction as the abstract (regex floor + optional model).
    const regexPass = redact(raw);
    let text = regexPass.text;
    if (adapters.redact) {
      try {
        text = (await adapters.redact(regexPass.text)).text;
      } catch {
        /* keep the regex result */
      }
    }
    const trimmed = text.trim().slice(0, USER_INTENT_MAX_CHARS);
    return trimmed.length > 0 ? trimmed : undefined;
  } catch {
    return undefined;
  }
}

// ── Helpers ─────────────────────────────────────────────────────

/** Substrings that mark a frustrated/blocked mood in a cognition emotion
 * tag (matched case-insensitively, so inflections like "frustrated" /
 * "frustration" hit). A generic linguistic cue set — NOT domain/tool vocab. */
const FRUSTRATION_MARKERS = [
  "frustrat",
  "annoy",
  "stuck",
  "confus",
  "irritat",
  "block",
  "stress",
  "angr",
  "overwhelm",
] as const;

/** Privacy-preserving per-segment behavioral signal — COUNTS/RATIOS ONLY.
 * `user_turns`: developer messages in the slice. `correction_count`: user
 * messages that land right after an assistant message (a re-prompt /
 * correction proxy). `frustration`: 0-1, raised by re-prompt density and by
 * negative cognition mood tags. Never includes raw text. */
function computeBehavior(
  slice: RawEvent[],
  cognition: CognitionTags | null,
): { user_turns: number; correction_count: number; frustration: number } {
  let userTurns = 0;
  let correctionCount = 0;
  let prevWasAssistant = false;
  for (const ev of slice) {
    if (ev.kind === "user_message") {
      userTurns++;
      if (prevWasAssistant) correctionCount++;
      prevWasAssistant = false;
    } else if (ev.kind === "assistant_message") {
      prevWasAssistant = true;
    }
  }
  const frustratedMood =
    cognition?.emotions?.some((e) => {
      const lower = e.toLowerCase();
      return FRUSTRATION_MARKERS.some((m) => lower.includes(m));
    }) ?? false;
  const frustration = Math.min(
    1,
    Math.max(correctionCount / 4, frustratedMood ? 0.8 : 0),
  );
  return {
    user_turns: userTurns,
    correction_count: correctionCount,
    frustration: Math.round(frustration * 100) / 100,
  };
}

/**
 * Pick representative turn excerpts from a slice and re-redact them
 * before they reach the summariser prompt. Strategy:
 *   • Always include the first event with content (the user's intent).
 *   • Always include the last event with content (the resolution).
 *   • Up to 3 evenly-spaced events in between.
 *   • Skip events without content_excerpt — silent fallback to
 *     metadata-only behaviour for older parsers.
 * Re-runs regex redaction on every excerpt as defence-in-depth even
 * though the parser is supposed to have done it already.
 */
function sampleAndRedactExcerpts(slice: RawEvent[]): string[] {
  const withContent: Array<{ idx: number; text: string }> = [];
  for (let i = 0; i < slice.length; i++) {
    const c = slice[i]?.content_excerpt;
    if (c && c.trim().length > 0) withContent.push({ idx: i, text: c });
  }
  if (withContent.length === 0) return [];

  const picks: number[] = [0]; // first
  if (withContent.length > 1) picks.push(withContent.length - 1); // last
  for (const frac of [0.25, 0.5, 0.75]) {
    const idx = Math.floor(withContent.length * frac);
    if (!picks.includes(idx)) picks.push(idx);
    if (picks.length >= 5) break;
  }
  picks.sort((a, b) => a - b);

  const out: string[] = [];
  for (const i of picks) {
    const raw = withContent[i]?.text;
    if (!raw) continue;
    const redacted = redact(raw).text;
    out.push(redacted.slice(0, 200));
  }
  return out;
}

function turnSurface(e: RawEvent): string {
  const parts: string[] = [e.kind, e.agent];
  if (e.model) parts.push(e.model);
  const toolCalls = Object.keys(e.tool_calls ?? {});
  if (toolCalls.length) parts.push(`tools:${toolCalls.join(",")}`);
  if (e.files_touched?.length) parts.push(`files:${e.files_touched.length}`);
  return parts.join(" ");
}

function cosineDistance(a: number[], b: number[]): number {
  let dot = 0;
  let na = 0;
  let nb = 0;
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    dot += a[i]! * b[i]!;
    na += a[i]! * a[i]!;
    nb += b[i]! * b[i]!;
  }
  const denom = Math.sqrt(na) * Math.sqrt(nb);
  return denom > 0 ? 1 - dot / denom : 1;
}

function inferEnvironment(branch: string): string | null {
  const b = branch.toLowerCase();
  if (b === "main" || b === "master" || b.startsWith("release/")) return "Prod";
  if (b === "staging" || b.startsWith("staging/")) return "Staging";
  if (b === "dev" || b === "develop" || b.startsWith("dev/")) return "Dev";
  return null;
}

/** Local-time taxonomy hints for a slice (WHEN the work happened). The daemon
 *  runs on the engineer's OWN machine, so `getHours()`/`getDay()` give THEIR
 *  wall-clock — the server only ever sees UTC and could never derive these. The
 *  taxonomy temporal/cadence drivers turn these into Time-of-Day / Cadence nodes,
 *  and only for the buckets that actually occur (an engineer who never codes at
 *  night simply has no Night node). Friday is split out from the rest of the week
 *  on purpose — it's the relatable one. */
export function temporalHints(startedAtMs: number): Segment["tags"] {
  const d = new Date(startedAtMs);
  const h = d.getHours();
  const timeOfDay =
    h >= 5 && h < 12
      ? "Morning"
      : h >= 12 && h < 17
        ? "Midday"
        : h >= 17 && h < 21
          ? "Evening"
          : "Night";
  const day = d.getDay(); // 0 = Sun … 6 = Sat
  const cadence = day === 0 || day === 6 ? "Weekend" : day === 5 ? "Friday" : "Weekday";
  return [
    { root_key: "time_of_day", name: timeOfDay, confidence: 1 },
    { root_key: "cadence", name: cadence, confidence: 1 },
  ];
}

// Re-exports for convenience of consumers that previously imported
// redact + summariser types from daemon-core/pipeline.
export { type RedactionResult, redact } from "@modelstat/core/redact";
export {
  buildCognitionUserPrompt,
  COGNITION_MAX_TOKENS,
  COGNITION_SYSTEM_PROMPT,
  COGNITION_TEMPERATURE,
  type CognitionInput,
  type CognitionTags,
  type Cognizer,
  cognitionHints,
  EMPTY_COGNITION,
  formatCognitionSuffix,
  MAX_COGNITION_TAG_CHARS,
  MAX_COGNITION_TAGS_PER_FIELD,
  parseCognitionReply,
  sanitiseTags,
} from "./cognition.js";
export {
  BROWSER_EMBED_MODEL,
  OLLAMA_CHAT_MODEL,
  OLLAMA_EMBED_MODEL,
  QWEN_CHARS_PER_TOKEN,
  SUMMARISER_MAX_TOKENS,
  SUMMARISER_MODEL_FAMILY,
  SUMMARISER_SYSTEM_PROMPT,
  SUMMARISER_TEMPERATURE,
  SUMMARISER_TOP_K,
  WEBLLM_CHAT_MODEL,
} from "./prompts.js";
export {
  buildScriptSummaryUserPrompt,
  SCRIPT_SUMMARY_INPUT_MAX_CHARS,
  SCRIPT_SUMMARY_MAX_TOKENS,
  SCRIPT_SUMMARY_OUTPUT_MAX_CHARS,
  SCRIPT_SUMMARY_SYSTEM_PROMPT,
  SCRIPT_SUMMARY_TEMPERATURE,
  type ScriptSummarizer,
} from "./script-summary.js";
export {
  buildLinkExtractUserPrompt,
  buildSessionMetadata,
  LINK_EXTRACT_MAX_ABSTRACTS,
  LINK_EXTRACT_MAX_TOKENS,
  LINK_EXTRACT_SYSTEM_PROMPT,
  LINK_EXTRACT_TEMPERATURE,
  type LinkExtractInput,
  type LinkExtractor,
  type SessionMetadataOptions,
} from "./session-metadata.js";
export {
  buildSessionTitles,
  buildTitleUserPrompt,
  type Entitler,
  fallbackTitle,
  sampleAbstracts,
  sanitiseTitle,
  stripCognitionSuffix,
  TITLE_MAX_CHARS,
  TITLE_WIRE_MAX_CHARS,
  TITLER_MAX_ABSTRACTS,
  TITLER_MAX_TOKENS,
  TITLER_SYSTEM_PROMPT,
  TITLER_TEMPERATURE,
  type TitleInput,
} from "./title.js";
export {
  applyLlmRedactions,
  composeRedactors,
  LLM_REDACTION_MARKER,
  parseRedactReply,
  REDACT_MAX_TOKENS,
  REDACT_SYSTEM_PROMPT,
  REDACT_TEMPERATURE,
  shouldDeepRedact,
} from "./redaction.js";
