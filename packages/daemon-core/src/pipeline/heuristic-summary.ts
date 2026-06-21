/**
 * Deterministic, dependency-free extractive summariser — the always-works
 * fallback for when the bundled LLM can't run on this machine (GPU/CPU runtime
 * failed to load, the ~2.7 GB GGUF isn't downloaded yet, no network on first
 * scan, an incompatible prebuilt binary). Instead of BLOCKING ingest — the old
 * behaviour, which refused to start the daemon — we ship a real, if plainer,
 * abstract built by extracting the user's apparent intent from the sampled
 * excerpts and appending the structural facts.
 *
 * This is intentionally NOT an LLM: no model, no native code, no I/O. It always
 * produces output, so the daemon never stalls. It IS clearly lower quality than
 * the Qwen summariser, so the daemon runs it in a flagged "degraded" mode and
 * re-summarises these segments at model quality once the LLM is available again
 * (see the self-heal in apps/daemon/src/daemon.ts).
 *
 * Crucially it does NOT emit the metadata-only placeholder ("100 turns on
 * claude_code") the LLM path guards against: it leads with real extracted prose
 * from the conversation, so downstream classification accepts it.
 */
import { ABSTRACT_OUTPUT_MAX_CHARS } from "./prompts.js";
import type { Summarizer } from "./index.js";

// Short conversational openers that carry no task signal — skipped when picking
// the lead so the abstract starts with what the user was actually doing.
const GREETING = /^(hi|hey|hello|thanks?|thank you|ok(ay)?|yes|no|sure|please|cool|great|nice)\b[\s!.,]*/i;
// Leading politeness/filler stripped from the lead so it reads as an action.
const LEAD_FILLER =
  /^(can you|could you|could we|can we|please|i(?:'d| would)? (?:like|want)(?: you)? to|i need(?: you)? to|let'?s|help me|i'?m trying to|i am trying to|now|so|ok(?:ay)?)\s+/i;

/** Pull the `[turn N] "…"` excerpt bodies back out of a built summariser prompt —
 * the fallback when a caller didn't pass structured `excerpts` (e.g. the browser
 * summarisers share the same Summarizer shape). */
function parsePromptExcerpts(prompt: string): string[] {
  const out: string[] = [];
  const re = /\[turn \d+\]\s*"([^"]*)"/g;
  let m: RegExpExecArray | null = re.exec(prompt);
  while (m) {
    if (m[1]) out.push(m[1]);
    m = re.exec(prompt);
  }
  return out;
}

/** The `Session context: …` facts line from a built prompt, if present. */
function parsePromptFacts(prompt: string): string {
  const m = /Session context:\s*(.+?)\.\s*(?:\n|$)/.exec(prompt);
  return m?.[1]?.trim() ?? "";
}

/** First substantive excerpt (the intent), preferring the earliest non-greeting
 * line of real length; falls back to the longest early line, then anything. */
function pickIntent(lines: string[]): string | null {
  const early = lines.slice(0, 5);
  const substantive = early.filter((l) => l.length >= 16 && !GREETING.test(l));
  if (substantive.length > 0) return substantive[0]!;
  // Nothing substantive up front — take the longest early line so we still lead
  // with content rather than a bare "hi".
  const byLen = [...early].sort((a, b) => b.length - a.length);
  return byLen[0] ?? lines.find((l) => l.length > 0) ?? null;
}

/** Trim leading politeness/filler and capitalise so the lead reads as an action
 * phrase ("fix the Metal crash" → "Fix the Metal crash"). */
function cleanLead(s: string): string {
  let t = s.replace(GREETING, "").trim();
  t = t.replace(LEAD_FILLER, "").trim();
  if (!t) t = s.trim();
  return t.charAt(0).toUpperCase() + t.slice(1);
}

function clamp(s: string, max: number): string {
  const t = s.replace(/\s+/g, " ").trim();
  if (t.length <= max) return t;
  // Cut on a word boundary just under the cap, then add an ellipsis.
  const cut = t.slice(0, max - 1);
  const sp = cut.lastIndexOf(" ");
  return `${(sp > max * 0.6 ? cut.slice(0, sp) : cut).trimEnd()}…`;
}

/**
 * Build the always-works extractive summariser. Uses the structured `excerpts`
 * + `facts` the pipeline passes; falls back to parsing them out of `prompt` for
 * callers that only provide the built prompt string. Never throws, never empty.
 */
export function heuristicSummarize(): Summarizer {
  return async ({ prompt, excerpts, facts }) => {
    const lines = (excerpts && excerpts.length > 0 ? excerpts : parsePromptExcerpts(prompt))
      .map((s) => s.replace(/\s+/g, " ").trim())
      .filter((s) => s.length > 0);
    const factText = (facts?.trim() || parsePromptFacts(prompt)).replace(/\s+/g, " ").trim();

    const intent = pickIntent(lines);
    const lead = intent ? cleanLead(intent) : "AI coding session";
    // Append the structural facts as bracketed context — repo, turn count,
    // files, tools — so the abstract still carries the metadata signal.
    const body = factText ? `${lead} [${factText}]` : lead;
    return clamp(body, ABSTRACT_OUTPUT_MAX_CHARS);
  };
}
