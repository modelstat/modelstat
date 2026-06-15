/**
 * Prompt + contract for the on-device per-SCRIPT content summariser — the
 * fourth local-model pass.
 *
 * A tool command often runs script/bash FILES (`./deploy.sh`,
 * `scripts/migrate.py`, …). The redacted command alone tells the server the
 * file was run but not what it DOES. This pass reads each referenced file
 * locally and compacts it into one factual sentence, so the server understands
 * the command's real effect without ever seeing the file's contents.
 *
 * Browser-safe: prompt strings + a builder + the adapter type only — no `fs`, no
 * model. The Node binding (companion-core/node/llama.ts) supplies inference; the
 * agent's enrichment pass (apps/agent-dev) supplies file reading + redaction.
 *
 * Generic by construction: nothing about specific tools, languages, or
 * categories is baked in — the model describes whatever the file actually does.
 */

/** Hard cap on one script abstract — mirrors the wire field
 * `ToolAction.scripts[].summary` (`z.string().max(200)`). */
export const SCRIPT_SUMMARY_OUTPUT_MAX_CHARS = 200;

/** Low temperature: this is faithful description, not creative writing. */
export const SCRIPT_SUMMARY_TEMPERATURE = 0.2;

/** The answer is one short sentence; the Node binding adds a thinking budget on
 * top of this (same pattern as the cognition/title passes). */
export const SCRIPT_SUMMARY_MAX_TOKENS = 120;

/** How much of a script file we feed the model. Scripts are usually small; a few
 * KB is plenty to characterise intent and keeps the prompt inside the local
 * context window. The read side also caps bytes before calling. */
export const SCRIPT_SUMMARY_INPUT_MAX_CHARS = 6000;

export const SCRIPT_SUMMARY_SYSTEM_PROMPT = `You summarise what a script file does, for an engineer scanning a dashboard.

Rules:
- Output ONE plain sentence (at most 200 characters) stating what the script DOES when it runs.
- Be concrete and factual: the actions it performs and the systems it touches.
- No preamble ("This script…"), no markdown, no quotes, no line breaks.
- Do not invent behaviour that is not in the file. If the file is trivial or unreadable, say so briefly.
- Never include secrets, tokens, passwords, or personal data, even if they appear in the file.`;

/** Build the user turn: the script's reference (for context) + its capped
 * contents. Truncation here is a backstop — the agent already caps the bytes it
 * reads, but a single over-long line could still blow the context window. */
export function buildScriptSummaryUserPrompt(input: { ref: string; content: string }): string {
  const body = input.content.slice(0, SCRIPT_SUMMARY_INPUT_MAX_CHARS);
  return [
    `Script reference: ${input.ref}`,
    "Contents:",
    "```",
    body,
    "```",
    "",
    "One sentence (≤200 chars): what does running this script do?",
  ].join("\n");
}

/**
 * Adapter the agent supplies to turn a script's `{ ref, content }` into a
 * one-sentence abstract. Best-effort by contract: returns `null` on any failure
 * (model not loaded, empty answer) so a script that can't be summarised simply
 * ships without an abstract — the call still ships its redacted command.
 */
export type ScriptSummarizer = (input: { ref: string; content: string }) => Promise<string | null>;
