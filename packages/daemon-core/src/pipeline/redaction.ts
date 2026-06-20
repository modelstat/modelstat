/**
 * Local-LLM redaction backstop (defense-in-depth, layer 3).
 *
 * Layer 1 (`@modelstat/core/redact`, deterministic regex + entropy) and layer 2
 * (the on-device Privacy Filter NER model) run first. This third pass asks the
 * bundled local model to spot any *remaining* secret/PII it recognises that the
 * earlier layers' fixed patterns missed (novel key formats, contextual PII).
 *
 * SAFETY — the model never rewrites the text. It only *names* candidate
 * substrings; we then replace each **verbatim occurrence** deterministically
 * (see {@link applyLlmRedactions}). So the model can only ever ADD a redaction of
 * a string that genuinely appears — it cannot reword a command, invent a
 * redaction, or leak more. On any error/empty reply the text is returned
 * unchanged (the earlier layers already redacted it) — fail-safe, never
 * fail-open. Everything runs on-device; no text leaves the machine.
 */

import type { Redactor } from "./index.js";

/**
 * Chain redactors into one: each runs on the previous one's output, and their
 * `counts` merge. Each sub-redactor is independently fail-safe (returns its
 * input on error), and a thrown redactor is skipped — so the chain can only ever
 * redact MORE, never less, and never throws. Used to stack the Privacy Filter
 * (layer 2) and the local-LLM backstop (layer 3) behind one `redact` adapter.
 */
export function composeRedactors(...redactors: Redactor[]): Redactor {
  return async (text: string) => {
    let out = text;
    const counts: Record<string, number> = {};
    for (const r of redactors) {
      try {
        const res = await r(out);
        out = res.text;
        for (const [k, v] of Object.entries(res.counts)) counts[k] = (counts[k] ?? 0) + v;
      } catch {
        // fail-safe: keep the prior layer's output and continue
      }
    }
    return { text: out, counts };
  };
}

/** Placeholder for an LLM-flagged secret/PII span. */
export const LLM_REDACTION_MARKER = "[REDACTED:llm]";

/** Minimum length of an LLM-flagged substring we'll act on. Secrets/keys are
 * long; this avoids nuking short, common, high-value-for-analytics tokens (a
 * model that flags `prod` or `api` shouldn't blank them everywhere). Short PII
 * (names) is the Privacy Filter's job (layer 2). */
const MIN_CANDIDATE_CHARS = 8;

/** Infra/code words a model sometimes over-flags as "sensitive" — never redact
 * these even if returned (they carry real analytic signal and aren't secrets). */
const SAFE_WORDS = new Set([
  "production",
  "staging",
  "localhost",
  "endpoint",
  "database",
  "password", // the literal word (e.g. a flag name), not a value
  "secret",
  "token",
  "credential",
  "[redacted",
]);

export const REDACT_SYSTEM_PROMPT =
  "You are a security redaction reviewer. You are given a single shell command " +
  "that has already been partly redacted. Find any remaining SECRETS or sensitive " +
  "credentials still present in plaintext: API keys, access tokens, bearer tokens, " +
  "passwords, private keys, connection strings with credentials, or other " +
  "high-entropy secret values. Do NOT flag: program names, flags, file paths, " +
  "hostnames, service/environment names (prod, dev), or existing [REDACTED:...] " +
  "markers. Output ONLY the exact secret substrings, one per line, copied " +
  "verbatim character-for-character as they appear in the command. If there are " +
  "no remaining secrets, output exactly NONE. Output nothing else — no prose, no " +
  "explanation, no numbering.";

/** Token budget — the model reasons (`<think>`) then lists a few short lines. */
export const REDACT_MAX_TOKENS = 512;
export const REDACT_TEMPERATURE = 0;

/** Cheap pre-filter: only spend a model call on commands that *could* still
 * carry a secret the earlier layers missed. Skips the overwhelming majority
 * (`git status`, `ls`, `cd …`) so the LLM pass is affordable per-call and in a
 * full reprocess. */
export function shouldDeepRedact(text: string): boolean {
  if (!text) return false;
  if (/[=]|--|:\/\/|@|\bbearer\b|token|secret|password|passwd|credential|api[_-]?key|private[_-]?key/i.test(text)) {
    return true;
  }
  // A long unbroken token is a generic key/blob signal.
  return /[A-Za-z0-9/+_-]{20,}/.test(text);
}

/** Parse the model's reply into candidate secret substrings. */
export function parseRedactReply(raw: string): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const line of raw.split("\n")) {
    const s = line.trim().replace(/^["'`]+|["'`]+$/g, "");
    if (!s || s.toUpperCase() === "NONE") continue;
    if (s.length < MIN_CANDIDATE_CHARS) continue;
    if (SAFE_WORDS.has(s.toLowerCase())) continue;
    if (s.startsWith("[REDACTED")) continue;
    if (seen.has(s)) continue;
    seen.add(s);
    out.push(s);
  }
  return out;
}

/** Escape a literal string for use in a RegExp. */
function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Replace every verbatim occurrence of each model-flagged candidate with
 * {@link LLM_REDACTION_MARKER}. Only candidates that actually appear in `text`
 * (and clear the length/safe-word guards in {@link parseRedactReply}) are
 * applied, so this can only ever ADD a redaction of a real substring.
 */
export function applyLlmRedactions(
  text: string,
  candidates: readonly string[],
): { text: string; count: number } {
  let out = text;
  let count = 0;
  // Longest-first so a candidate that contains another doesn't get partially
  // pre-redacted out from under the longer one.
  for (const cand of [...candidates].sort((a, b) => b.length - a.length)) {
    if (!out.includes(cand)) continue;
    const before = out;
    out = out.replace(new RegExp(escapeRe(cand), "g"), LLM_REDACTION_MARKER);
    if (out !== before) count += 1;
  }
  return { text: out, count };
}
