/**
 * Emotion + meta-cognition pass — a small client-side LLM call that
 * runs alongside the summariser and tags each segment with the user's
 * mood and cognitive mode.
 *
 *   emotions  →  short lowercase mood tags ("frustrated", "curious",
 *                "excited", "focused", "confused", "satisfied", …).
 *   meta      →  short lowercase meta-cognitive mode tags ("debugging",
 *                "exploring", "planning", "designing", "refactoring",
 *                "learning", "deciding", "reviewing", …).
 *
 * The list is intentionally NOT an enum. The point of the feature is
 * to let whatever vocabulary actually shows up in the work surface
 * organically rather than forcing it into a fixed set.
 *
 * Output shape (from the LLM):
 *
 *   { "emotions": ["frustrated", "curious"], "meta": ["debugging"] }
 *
 * The daemon ALSO appends a human-readable suffix to the abstract
 * sent to the server — `[Mood: frustrated, curious] [Mind: debugging]`
 * — so the cognition vocabulary travels inside the abstract text. The
 * structured field on the wire exists for cheap dashboard filtering /
 * aggregation.
 *
 * Failure mode is GRACEFUL — this is a fun / auxiliary pass, not part
 * of the core pipeline contract. If every runtime is unavailable, the
 * segment ships with `cognition` undefined and no suffix. The core
 * "summariser/classifier failures must throw" rule (see CLAUDE.md
 * memory) does not extend here; cognition is best-effort.
 */

/**
 * Canonical system prompt — the SAME for every runtime (Prompt API,
 * WebLLM, Ollama, future ones). Centralised here so changing the
 * vocabulary is one edit.
 */
export const COGNITION_SYSTEM_PROMPT =
  "You read a one-sentence summary of an AI-coding work session and identify the user's emotional state, mental mode, and working stance. " +
  "Output JSON only — first character of reply is `{`. Schema: " +
  '{"emotions":[],"meta":[],"posture":[]}. ' +
  "emotions: ≤ 3 short lowercase MOOD tags — how the user FEELS — such as " +
  "frustrated, curious, excited, calm, confused, anxious, satisfied, proud, happy, worried, disappointed, overwhelmed, confident. " +
  "meta: ≤ 3 short lowercase MENTAL-MODE tags — HOW the user is THINKING, never what they are doing. Valid examples: " +
  "focused, scattered, in-flow, deliberate, hurried, stuck, open, exploratory, methodical, distracted. " +
  "DO NOT emit ACTIVITY verbs (debugging, refactoring, designing, reviewing, deploying, planning, documenting, implementing) under meta — those describe the WORK, not the MIND. " +
  "If the only candidate tag would be an activity verb, return [] for meta instead. " +
  "posture: ≤ 2 short lowercase WORKING-STANCE tags — the user's risk appetite and how they treat the work, " +
  "inferred from how boldly or carefully they move — such as " +
  "ship-it, yolo, direct-to-prod, cautious, careful, methodical, questioning, skeptical, demanding, easygoing, trusting. " +
  "Each tag ≤ 24 chars, single word or hyphenated, no punctuation. " +
  "Only emit a tag if the summary gives clear evidence — return [] for any field when unsure. " +
  "Do not invent emotions, mental modes, or stances the user did not display. No prose, no markdown.";

/** Hard caps applied client-side regardless of what the LLM emits. */
export const MAX_COGNITION_TAGS_PER_FIELD = 3;
export const MAX_COGNITION_TAG_CHARS = 24;
export const COGNITION_MAX_TOKENS = 80;
export const COGNITION_TEMPERATURE = 0.2;

export interface CognitionTags {
  emotions: string[];
  meta: string[];
  /** Working-stance tags — the user's risk appetite / how they treat the work
   * (ship-it, cautious, questioning, easygoing). Surfaced as the `Posture`
   * taxonomy dimension. */
  posture: string[];
}

export const EMPTY_COGNITION: CognitionTags = { emotions: [], meta: [], posture: [] };

/**
 * The one input every Cognizer adapter accepts. Kept tiny on purpose:
 * the cognition pass operates on the already-summarised abstract, NOT
 * the raw turn excerpts — that work is the summariser's job. Reusing
 * the abstract keeps each call cheap and makes the cognition signal
 * downstream-of-redaction by construction.
 */
export interface CognitionInput {
  /** The pre-redacted segment abstract — same string the server will
   * eventually see. ≤ 512 chars. */
  abstract: string;
}

/**
 * Adapter contract. Returns sanitised tags or null when the runtime
 * couldn't produce anything (e.g. JSON parse error, model not loaded).
 * Callers should treat null as "no signal" — same as undefined.
 */
export type Cognizer = (input: CognitionInput) => Promise<CognitionTags | null>;

// ── User-prompt builder + reply parser ───────────────────────────────

/** Build the user message for the cognition LLM call. Pure / cheap so
 * tests can assert on the exact prompt without spinning up a runtime. */
export function buildCognitionUserPrompt(abstract: string): string {
  return `Summary: "${abstract.replace(/\s+/g, " ").trim().slice(0, 480)}"

Output JSON only.`;
}

/**
 * Parse + sanitise an LLM reply. Tolerates the model wrapping the JSON
 * in ```json fences``` or leading prose — finds the first `{ … }`
 * block via a balanced-brace scan. Returns null when nothing parses
 * cleanly.
 */
export function parseCognitionReply(text: string): CognitionTags | null {
  const json = extractFirstJsonObject(text);
  if (!json) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(json);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== "object") return null;
  const obj = parsed as { emotions?: unknown; meta?: unknown; posture?: unknown };
  return {
    emotions: sanitiseTags(obj.emotions),
    meta: sanitiseTags(obj.meta),
    posture: sanitiseTags(obj.posture),
  };
}

/**
 * Lowercase, trim, drop punctuation/numerics, dedupe, cap length and
 * count. Applied client-side to whatever the LLM emits so a confused
 * runtime can't ship "VERY ANGRY!!!" or 47 tags.
 */
export function sanitiseTags(raw: unknown): string[] {
  if (!Array.isArray(raw)) return [];
  const seen = new Set<string>();
  const out: string[] = [];
  for (const t of raw) {
    if (typeof t !== "string") continue;
    const cleaned = t
      .toLowerCase()
      .normalize("NFKD")
      // Keep letters + digits + hyphens. Drop everything else.
      .replace(/[^a-z0-9-]/g, "")
      .replace(/-+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, MAX_COGNITION_TAG_CHARS);
    if (!cleaned) continue;
    if (seen.has(cleaned)) continue;
    seen.add(cleaned);
    out.push(cleaned);
    if (out.length >= MAX_COGNITION_TAGS_PER_FIELD) break;
  }
  return out;
}

/** Format the human-readable suffix appended to the abstract.
 *
 *   formatCognitionSuffix({ emotions: ["frustrated","curious"], meta: ["debugging"] })
 *   → "[Mood: frustrated, curious] [Mind: debugging]"
 *
 * Returns "" when both arrays are empty so callers can append
 * unconditionally without injecting trailing whitespace.
 */
export function formatCognitionSuffix(c: CognitionTags | null | undefined): string {
  if (!c) return "";
  const parts: string[] = [];
  if (c.emotions.length > 0) parts.push(`[Mood: ${c.emotions.join(", ")}]`);
  if (c.meta.length > 0) parts.push(`[Mind: ${c.meta.join(", ")}]`);
  if (c.posture.length > 0) parts.push(`[Stance: ${c.posture.join(", ")}]`);
  return parts.join(" ");
}

/**
 * Structured `mood` + `mind` + `posture` hints for the segment's `tags` array.
 * Emits the PRIMARY (first) tag of each so the server's Mood/Mind/Posture drivers
 * create ONE leaf per session — mirroring the one-node-per-segment temporal/cadence
 * drivers. The full set still travels in the human-readable suffix. Capitalised for
 * display ("frustrated" → "Frustrated"). Returns [] when there's nothing to emit, so
 * the caller can `tags.push(...cognitionHints(c))` unconditionally.
 */
export function cognitionHints(
  c: CognitionTags | null | undefined,
): Array<{ root_key: string; name: string; confidence: number }> {
  if (!c) return [];
  const cap = (s: string) => (s ? s.charAt(0).toUpperCase() + s.slice(1) : s);
  const out: Array<{ root_key: string; name: string; confidence: number }> = [];
  if (c.emotions[0]) out.push({ root_key: "mood", name: cap(c.emotions[0]), confidence: 0.7 });
  if (c.meta[0]) out.push({ root_key: "mind", name: cap(c.meta[0]), confidence: 0.7 });
  if (c.posture[0]) out.push({ root_key: "posture", name: cap(c.posture[0]), confidence: 0.7 });
  return out;
}

/**
 * Extract the FIRST balanced `{…}` block from a string. Robust to the
 * common LLM mistakes: leading "```json" fences, trailing prose,
 * unescaped quotes inside string literals (which we don't try to
 * tolerate — JSON.parse handles those).
 */
function extractFirstJsonObject(s: string): string | null {
  const start = s.indexOf("{");
  if (start < 0) return null;
  let depth = 0;
  let inStr = false;
  let escape = false;
  for (let i = start; i < s.length; i++) {
    const ch = s[i];
    if (inStr) {
      if (escape) escape = false;
      else if (ch === "\\") escape = true;
      else if (ch === '"') inStr = false;
      continue;
    }
    if (ch === '"') {
      inStr = true;
      continue;
    }
    if (ch === "{") depth++;
    else if (ch === "}") {
      depth--;
      if (depth === 0) return s.slice(start, i + 1);
    }
  }
  return null;
}
