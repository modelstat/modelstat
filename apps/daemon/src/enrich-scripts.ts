/**
 * On-device per-script content summaries.
 *
 * A tool command often runs script/bash FILES (`./deploy.sh`,
 * `scripts/migrate.py`, …). `command_redacted` tells the backend the file was
 * run but not what it DOES. This pass reads each referenced file locally,
 * compacts it to one factual sentence with the bundled model, and attaches the
 * (redacted) abstract to `ToolAction.scripts` as `{ token, summary }`.
 *
 * PRIVACY: the raw command and the file CONTENTS never leave the device. Only
 * the model's one-sentence abstract ships, and only after `redact()` strips any
 * secret/PII it might still contain. `token` is the script's token AS IT APPEARS
 * IN `command_redacted` (verified to be a substring), so the backend zips each
 * abstract to its place in the redacted command deterministically — no fuzzy
 * matching, no raw paths.
 *
 * Best-effort + additive: any failure (file missing, unreadable, model error)
 * just leaves that script out; the call still ships its redacted command. The
 * whole pass can be skipped (browser daemon, model unavailable) with no loss
 * beyond the abstracts.
 *
 * Generic by construction: detection is a pure structural signal (script-ish
 * extension / explicit path), resolution tries liberal candidate roots, and the
 * model describes whatever the file actually does — no tool/category vocabulary.
 */

import type { ScriptSummarizer } from "@modelstat/daemon-core/pipeline";
import type { ToolCallDraft } from "@modelstat/daemon-core/queue";
import { redact, type ToolAction } from "@modelstat/core";
import { detectScriptRefs, type LocalToolContext, resolveScriptPath } from "@modelstat/parsers";

/** Wire cap on `ToolAction.scripts` (z.array(...).max(8)). */
const MAX_SCRIPTS_PER_CALL = 8;
/** Wire cap on `ToolAction.scripts[].summary` (z.string().max(200)). */
const MAX_SUMMARY_CHARS = 200;

export interface EnrichScriptsDeps {
  /** Turns a script's `{ ref, content }` into a one-sentence abstract (or
   * null). Best-effort — see `ScriptSummarizer`. */
  summarize: ScriptSummarizer;
  /** Existence probe — `fs.existsSync` in prod, a fake in tests. */
  exists: (path: string) => boolean;
  /** Read a resolved script's text. The caller is expected to cap bytes; an
   * over-long body is also sliced inside the prompt builder. */
  readFile: (path: string) => Promise<string>;
  /** Candidate root dirs from a call's cwd. Defaults to {@link defaultRoots}. */
  roots?: (cwd: string | null) => string[];
}

/**
 * Liberal, generic candidate roots from a cwd: the dir itself, a few ancestors
 * (monorepo roots / `../` refs), and a handful of conventional script subdirs so
 * a BARE `deploy.sh` still resolves to `<cwd>/scripts/deploy.sh`. No
 * project-structure assumptions beyond "scripts tend to live near cwd". The
 * resolver tries these most-absolute / longest-first, so the most specific
 * existing file wins.
 */
export function defaultRoots(cwd: string | null): string[] {
  if (!cwd) return [];
  const seg = cwd.replace(/\/+$/, "").split("/");
  const ancestors: string[] = [];
  for (let i = seg.length; i >= Math.max(1, seg.length - 3); i--) {
    ancestors.push(seg.slice(0, i).join("/") || "/");
  }
  const subdirs = ["scripts", "bin", "tools", "ci", ".github/scripts"];
  const roots = [...ancestors];
  for (const a of ancestors) for (const s of subdirs) roots.push(`${a}/${s}`);
  return roots;
}

/**
 * Enrich each draft's `ToolAction.scripts` in place from the matching local
 * context (raw command + cwd). Mutates drafts; returns nothing. Never throws —
 * per-call failures are swallowed so enrichment can't sink an upload.
 */
export async function enrichToolCallScripts(
  drafts: readonly ToolCallDraft[],
  contexts: readonly LocalToolContext[],
  deps: EnrichScriptsDeps,
): Promise<void> {
  if (contexts.length === 0) return;
  const ctxById = new Map(contexts.map((c) => [c.external_call_id, c]));
  for (const draft of drafts) {
    if (!draft.action) continue;
    const ctx = ctxById.get(draft.external_call_id);
    if (!ctx) continue;
    try {
      await enrichOneAction(draft.action, ctx, deps);
    } catch {
      // Additive + best-effort: a single bad script never sinks the draft.
    }
  }
}

/** Fill one action's `scripts` from its raw command + cwd. */
async function enrichOneAction(
  action: ToolAction,
  ctx: LocalToolContext,
  deps: EnrichScriptsDeps,
): Promise<void> {
  // No redacted command → nothing on the wire to anchor a token to.
  const redactedCommand = action.command_redacted;
  if (!redactedCommand) return;

  const refs = detectScriptRefs(ctx.command);
  if (refs.length === 0) return;

  const roots = (deps.roots ?? defaultRoots)(ctx.cwd);
  const seen = new Set<string>();
  const out: Array<{ token: string; summary: string }> = [];

  for (const ref of refs) {
    if (out.length >= MAX_SCRIPTS_PER_CALL) break;

    // The token must be EXACTLY a substring of command_redacted so the backend
    // can zip it deterministically. Redact the ref the same way the command was
    // redacted (same cwd), then require the result to actually appear in the
    // redacted command. A ref whose path was masked to a `[REDACTED:…]`
    // placeholder (out-of-repo absolute, secret-bearing) has no stable, unique
    // token — skip it rather than ship an unlinkable / sensitive abstract.
    const token = redact(ref, ctx.cwd ?? undefined).text.trim();
    if (!token || token.startsWith("[REDACTED") || seen.has(token)) continue;
    if (!redactedCommand.includes(token)) continue;

    // Resolve on disk (most-absolute / longest candidate first) + read.
    const path = resolveScriptPath(ref, roots, deps.exists);
    if (!path) continue;
    let content: string;
    try {
      content = await deps.readFile(path);
    } catch {
      continue;
    }
    if (!content.trim()) continue;

    const summaryRaw = await deps.summarize({ ref, content });
    if (!summaryRaw) continue;
    // Defence-in-depth: the model could echo a secret/PII from the file body.
    // Redact the abstract before it ships, then enforce the wire cap.
    const summary = redact(summaryRaw).text.trim().slice(0, MAX_SUMMARY_CHARS);
    if (!summary) continue;

    seen.add(token);
    out.push({ token, summary });
  }

  if (out.length > 0) action.scripts = out;
}
