/**
 * Runtime-agnostic LLM passes — the ONE place each pipeline pass
 * (summarise, cognition, title, link-extract, script-summary) is
 * implemented, parameterised over a {@link ChatComplete} transport.
 *
 * Every Node runtime (the bundled `node-llama-cpp` path in
 * ../node/llama.ts, the remote OpenAI-compatible path in
 * ../node/openai-compat.ts) speaks the same prompt contracts (see the
 * sibling ./cognition.ts, ./title.ts, … modules) and differs ONLY in how
 * it runs a single chat completion. So instead of each runtime
 * re-deriving "guard the input → build the user prompt → run one
 * completion → strip/parse → null-or-throw", it provides a `ChatComplete`
 * and the factories below own the rest. A prompt-contract or output-cap
 * change then lands on every runtime at once, by construction.
 *
 * The seam: a `ChatComplete` takes a system/user pair plus the answer
 * budget and returns the assistant's answer text with any `<think>`
 * reasoning block ALREADY stripped (the runtime owns stripping because
 * it owns the transport — llama strips the raw session output, the remote
 * port strips the parsed HTTP reply). The headroom a reasoning model
 * needs for that `<think>` block is a per-pass budget concern and lives
 * here (`THINKING_HEADROOM_TOKENS`).
 */

import {
  buildCognitionUserPrompt,
  COGNITION_MAX_TOKENS,
  COGNITION_SYSTEM_PROMPT,
  COGNITION_TEMPERATURE,
  type Cognizer,
  parseCognitionReply,
} from "./cognition.js";
import type { Summarizer } from "./index.js";
import { SUMMARISER_SYSTEM_PROMPT, SUMMARISER_TEMPERATURE } from "./prompts.js";
import {
  buildScriptSummaryUserPrompt,
  SCRIPT_SUMMARY_MAX_TOKENS,
  SCRIPT_SUMMARY_OUTPUT_MAX_CHARS,
  SCRIPT_SUMMARY_SYSTEM_PROMPT,
  SCRIPT_SUMMARY_TEMPERATURE,
  type ScriptSummarizer,
} from "./script-summary.js";
import {
  buildLinkExtractUserPrompt,
  LINK_EXTRACT_MAX_TOKENS,
  LINK_EXTRACT_SYSTEM_PROMPT,
  LINK_EXTRACT_TEMPERATURE,
  type LinkExtractor,
} from "./session-metadata.js";
import { THINKING_HEADROOM_TOKENS } from "./thinking.js";
import {
  buildTitleUserPrompt,
  type Entitler,
  TITLER_MAX_TOKENS,
  TITLER_SYSTEM_PROMPT,
  TITLER_TEMPERATURE,
} from "./title.js";

/** One chat completion. `system`/`user` are the two turns; `temperature`
 * and `maxTokens` bound the answer. Returns the assistant answer with any
 * `<think>` block ALREADY stripped by the runtime. Throws on transport
 * failure (the summariser pass propagates it; best-effort passes catch). */
export interface ChatRequest {
  readonly system: string;
  readonly user: string;
  readonly temperature: number;
  readonly maxTokens: number;
}
export type ChatComplete = (req: ChatRequest) => Promise<string>;

/** Token budget for one summarise call. Reasoning models routinely burn
 * 400-800 tokens on the `<think>` pass before the answer; ceil generous
 * so they don't starve. The output is sliced to {@link SUMMARY_OUTPUT_MAX_CHARS}
 * AFTER stripping, not bounded by this budget. Non-thinking models stop
 * at EOS well before it. The pipeline's caller passes a much smaller
 * `maxTokens` estimate which the summariser pass deliberately IGNORES. */
export const SUMMARY_MAX_TOKENS = 1024;
/** Hard char cap on the returned abstract — leaves room under the
 * 512-char Segment cap for caller-added context (e.g. "work on ${repo}"). */
export const SUMMARY_OUTPUT_MAX_CHARS = 240;

/**
 * Summariser pass — REQUIRED output. Runs one completion over the
 * pre-built prompt and returns a ≤240-char abstract. Throws when the
 * model yields nothing (e.g. it spent the whole budget on `<think>`, or
 * the chat template is wrong) so the resilient wrapper in apps/daemon can
 * degrade to the extractive fallback. The caller's `maxTokens` estimate
 * is ignored in favour of {@link SUMMARY_MAX_TOKENS}.
 */
export function makeSummarizer(chat: ChatComplete): Summarizer {
  return async ({ prompt }) => {
    const out = await chat({
      system: SUMMARISER_SYSTEM_PROMPT,
      user: prompt,
      temperature: SUMMARISER_TEMPERATURE,
      maxTokens: SUMMARY_MAX_TOKENS,
    });
    if (out.length === 0) {
      throw new Error(
        "summariser returned no answer text after stripping <think> blocks — the thinking " +
          "budget may be too low or the model template is misconfigured.",
      );
    }
    return out.slice(0, SUMMARY_OUTPUT_MAX_CHARS);
  };
}

/** Cognition pass — best-effort mood/mind/posture tags from the
 * already-redacted abstract. Sub-threshold input or any failure ⇒ null
 * (segment ships without the suffix). The guard short-circuits BEFORE the
 * port so no completion is run for a trivial abstract. */
export function makeCognizer(chat: ChatComplete): Cognizer {
  return async ({ abstract }) => {
    if (!abstract || abstract.trim().length < 12) return null;
    try {
      const out = await chat({
        system: COGNITION_SYSTEM_PROMPT,
        user: buildCognitionUserPrompt(abstract),
        temperature: COGNITION_TEMPERATURE,
        maxTokens: COGNITION_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      return parseCognitionReply(out);
    } catch {
      return null;
    }
  };
}

/** Session-title pass — best-effort, one short completion over the
 * redacted abstracts. Empty input or any failure ⇒ null and the caller
 * falls back to a deterministic title. */
export function makeEntitler(chat: ChatComplete): Entitler {
  return async (input) => {
    if (input.abstracts.length === 0) return null;
    try {
      const out = await chat({
        system: TITLER_SYSTEM_PROMPT,
        user: buildTitleUserPrompt(input),
        temperature: TITLER_TEMPERATURE,
        maxTokens: TITLER_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      return out || null;
    } catch {
      return null;
    }
  };
}

/** Link-extraction pass — best-effort, surfaces PR/issue/commit refs from
 * the redacted abstracts as free text the caller re-parses
 * deterministically. Empty input or any failure ⇒ null. */
export function makeLinkExtractor(chat: ChatComplete): LinkExtractor {
  return async ({ abstracts }) => {
    if (abstracts.length === 0) return null;
    try {
      const out = await chat({
        system: LINK_EXTRACT_SYSTEM_PROMPT,
        user: buildLinkExtractUserPrompt(abstracts),
        temperature: LINK_EXTRACT_TEMPERATURE,
        maxTokens: LINK_EXTRACT_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      return out || null;
    } catch {
      return null;
    }
  };
}

/** Per-script content-summary pass — best-effort one-sentence abstract of
 * what running a script FILE does. Empty content or any failure ⇒ null
 * and the call ships without a script abstract. */
export function makeScriptSummarizer(chat: ChatComplete): ScriptSummarizer {
  return async ({ ref, content }) => {
    if (!content || content.trim().length === 0) return null;
    try {
      const out = await chat({
        system: SCRIPT_SUMMARY_SYSTEM_PROMPT,
        user: buildScriptSummaryUserPrompt({ ref, content }),
        temperature: SCRIPT_SUMMARY_TEMPERATURE,
        maxTokens: SCRIPT_SUMMARY_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      const oneLine = out.replace(/\s+/g, " ").trim();
      return oneLine ? oneLine.slice(0, SCRIPT_SUMMARY_OUTPUT_MAX_CHARS) : null;
    } catch {
      return null;
    }
  };
}
