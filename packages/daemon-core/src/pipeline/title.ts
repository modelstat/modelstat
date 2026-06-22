/**
 * Session-title pass — one tiny client-side LLM call per session that
 * turns the session's segment abstracts into a short, scannable title
 * for the dashboard's sessions list ("what was this session about?").
 *
 *   input   →  the session's per-segment abstracts (already produced
 *              by the summariser, already redacted) + cheap metadata.
 *   output  →  ≤ TITLE_MAX_CHARS title naming the dominant theme; when
 *              a session clearly spans several themes the model names
 *              the top areas (max 3) separated by " · ".
 *
 * Zero marginal dollar cost by construction: it reuses the SAME local
 * model that already summarised the segments (bundled Qwen via
 * node-llama-cpp on the CLI), adding one short generation per session
 * per upload. The server never runs an LLM for this.
 *
 * Failure mode is GRACEFUL — like cognition, this is auxiliary to the
 * core pipeline contract. If the entitler is unavailable or returns
 * garbage, `buildSessionTitles` falls back to a deterministic title
 * derived from the first segment's abstract, so the batch always ships
 * and the dashboard always has something honest to show.
 */
import { redact } from "@modelstat/core/redact";
import type { Segment } from "@modelstat/core/schemas";

/**
 * Canonical system prompt — the SAME for every runtime, centralised
 * here like COGNITION_SYSTEM_PROMPT so the wording is one edit.
 */
export const TITLER_SYSTEM_PROMPT =
  "You write a short TITLE for an AI coding session, given one-sentence " +
  "summaries of its parts in chronological order. The title names what the " +
  "session was about at a high level, in 3-8 words. If the session clearly " +
  "spans more than one theme, name the top themes (at most 3) separated by " +
  "' · '. Use concrete domain keywords (features, components, subsystems). " +
  "No narration, no filler words like 'session' or 'work on', no quotes, " +
  "no trailing period, no PII, no code literals, no file paths. " +
  "Reply with only the title.";

/** Hard cap applied client-side to whatever the LLM emits. Also the
 * cap the dashboard's sessions list renders without truncation. */
export const TITLE_MAX_CHARS = 80;

/** Wire-format ceiling (IngestBatch.session_titles values). Above the
 * display cap so a future longer-title prompt fits without a schema
 * change — mirrors the ABSTRACT_MAX_CHARS / OUTPUT_MAX_CHARS split. */
export const TITLE_WIRE_MAX_CHARS = 120;

export const TITLER_MAX_TOKENS = 60;
export const TITLER_TEMPERATURE = 0.2;

/** At most this many abstracts are sampled into the title prompt.
 * First + last are always included (intent + latest state); the rest
 * are evenly spaced. Keeps the prompt ≤ ~2.5 KB — the same attention
 * sweet spot the segment summariser is tuned for. */
export const TITLER_MAX_ABSTRACTS = 10;

/** Per-abstract slice inside the title prompt. Abstracts are ≤ 240
 * chars already; this guards against the 512-char storage ceiling. */
export const TITLER_ABSTRACT_SLICE_CHARS = 240;

/**
 * The one input every Entitler adapter accepts. Operates on the
 * already-summarised, already-redacted abstracts — never raw turns —
 * so the title is downstream-of-redaction by construction.
 */
export interface TitleInput {
  /** Per-segment abstracts in chronological order, cognition suffix
   * already stripped. Non-empty. */
  abstracts: string[];
  /** Optional one-line metadata facts ("repo org/repo; 12 segments on
   * claude_code") to ground the title. */
  facts?: string | null;
}

/**
 * Adapter contract. Returns the raw model reply (pre-sanitise) or null
 * when the runtime couldn't produce anything. Callers treat null as
 * "no signal" and fall back deterministically.
 */
export type Entitler = (input: TitleInput) => Promise<string | null>;

// ── User-prompt builder + reply sanitiser ────────────────────────────

/** Build the user message for the title LLM call. Pure / cheap so
 * tests can assert on the exact prompt without spinning up a runtime. */
export function buildTitleUserPrompt(input: TitleInput): string {
  const lines = input.abstracts
    .map(
      (a, i) =>
        `  [part ${i + 1}] ${a.replace(/\s+/g, " ").trim().slice(0, TITLER_ABSTRACT_SLICE_CHARS)}`,
    )
    .join("\n");
  const facts = input.facts?.trim();
  return `${facts ? `Session context: ${facts}.\n\n` : ""}Summaries of the session's parts (chronological):
${lines}

Write the title.`;
}

/** Strip the `[Mood: …] [Mind: …] [Stance: …]` cognition suffix the pipeline
 * appends to abstracts — these describe the user, not the work, and would
 * otherwise leak into titles ("Frustrated debugging…"). */
export function stripCognitionSuffix(abstract: string): string {
  return abstract.replace(/\s*\[(?:Mood|Mind|Stance):[^\]]*\]/g, "").trim();
}

/**
 * Lowercase-noise cleanup applied to whatever the LLM emits: drop
 * surrounding quotes/backticks/fences, collapse whitespace, drop a
 * trailing period, re-redact as defence-in-depth, cap length. Returns
 * "" when nothing usable remains — callers then fall back.
 */
export function sanitiseTitle(raw: string | null | undefined): string {
  if (!raw) return "";
  // A "title" that turned into a paragraph is a failed generation —
  // keep only the first non-empty line, BEFORE whitespace collapsing
  // folds the newlines away.
  const firstLine =
    raw
      .replace(/```[a-z]*\n?/gi, "")
      .split(/[\n\r]/)
      .map((l) => l.trim())
      .find((l) => l.length > 0) ?? "";
  let t = firstLine
    .replace(/\s+/g, " ")
    // Models love wrapping titles in quotes despite instructions.
    .replace(/^["'`“”]+/, "")
    .replace(/["'`“”]+$/, "")
    .replace(/[.!]+$/, "")
    .trim();
  if (!t) return "";
  t = redact(t).text;
  return t.slice(0, TITLE_MAX_CHARS).trim();
}

/** Deterministic fallback — first segment's abstract (the session's
 * intent), cognition-stripped, cut at the first sentence boundary.
 * Honest and always available; never an LLM call. */
export function fallbackTitle(abstracts: string[]): string {
  const first = abstracts.find((a) => a.trim().length > 0);
  if (!first) return "";
  const sentence = first.split(/(?<=[.!?])\s/, 1)[0] ?? first;
  return sanitiseTitle(sentence);
}

/** Evenly sample up to `max` abstracts keeping first + last. */
export function sampleAbstracts(abstracts: string[], max = TITLER_MAX_ABSTRACTS): string[] {
  if (abstracts.length <= max) return abstracts;
  const picks = new Set<number>([0, abstracts.length - 1]);
  for (let i = 1; picks.size < max; i++) {
    picks.add(Math.round((i * (abstracts.length - 1)) / (max - 1)));
  }
  return [...picks].sort((a, b) => a - b).map((i) => abstracts[i]!);
}

/**
 * Build one title per session from a batch's segments.
 *
 * Groups `segments` by session_id (chronological within each), asks
 * the entitler once per session, sanitises, and falls back to the
 * deterministic title when the entitler is missing / fails / returns
 * noise. Sessions whose segments carry no usable abstract are omitted
 * — shipping an empty title would only overwrite a better one
 * server-side.
 *
 * Returns a map suitable for `IngestBatch.session_titles`.
 */
export async function buildSessionTitles(
  segments: Segment[],
  entitle?: Entitler,
): Promise<Record<string, string>> {
  const bySession = new Map<string, Segment[]>();
  for (const s of segments) {
    const arr = bySession.get(s.session_id) ?? [];
    arr.push(s);
    bySession.set(s.session_id, arr);
  }

  const out: Record<string, string> = {};
  for (const [sessionId, segs] of bySession) {
    const sorted = [...segs].sort((a, b) => a.started_at.localeCompare(b.started_at));
    const abstracts = sorted
      .map((s) => stripCognitionSuffix(s.abstract))
      .filter((a) => a.length > 0);
    if (abstracts.length === 0) continue;

    let title = "";
    if (entitle) {
      const first = sorted[0]!;
      const project = first.tags.find((t) => t.root_key === "projects")?.name;
      const facts = [
        project ? `repo ${project}` : null,
        `${sorted.length} part${sorted.length === 1 ? "" : "s"} on ${first.agent}`,
      ]
        .filter(Boolean)
        .join("; ");
      try {
        title = sanitiseTitle(await entitle({ abstracts: sampleAbstracts(abstracts), facts }));
      } catch {
        title = "";
      }
    }
    if (!title) title = fallbackTitle(abstracts);
    if (title) out[sessionId] = title;
  }
  return out;
}
