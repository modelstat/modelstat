/**
 * CLI-side pipeline binding — wires the summariser/embedder pair the
 * daemon runs and hands them to companion-core.
 *
 * ONE summariser path, no fallback: the bundled `node-llama-cpp`
 * runtime. The daemon ships `node-llama-cpp` as a real dependency and
 * stages it (plus this platform's prebuilt binary) beside the installed
 * bundle at install time — see apps/daemon/src/service.ts
 * `installNativeRuntime`. A ~2.7 GB Qwen3.5-4B GGUF is lazy-downloaded
 * to `~/.modelstat/models/` on first scan. This runs on every supported
 * machine (macOS arm64/x64, Linux) with no external runtime to install,
 * configure, or keep alive — no Ollama, no separate daemon, no probing.
 *
 * **No silent no-op fallback.** Summarisation is core product output:
 * if the native runtime can't load on this machine the daemon throws at
 * startup so the user sees the failure immediately, instead of happily
 * uploading thousands of useless "100 turns on claude_code" abstracts
 * that look fine in the database but tell them nothing.
 *
 * The adapters are built once and cached for the life of the process.
 */
import { existsSync } from "node:fs";
import { readFile as fsReadFile } from "node:fs/promises";
import {
  createTransformersJsEmbedder,
  defaultLlamaConfig,
  llamaCognize,
  llamaEntitle,
  llamaExtractLinks,
  llamaScriptSummarize,
  llamaSummarize,
} from "@modelstat/companion-core/node";
import {
  buildSegmentsForSession,
  buildSessionMetadata as buildSessionMetadataCore,
  buildSessionTitles as buildSessionTitlesCore,
  type PipelineAdapters,
  type ScriptSummarizer,
  type SegmentProgressFn,
} from "@modelstat/companion-core/pipeline";
import type { ToolCallDraft } from "@modelstat/companion-core/queue";
import { createPrivacyFilterRedactor } from "@modelstat/companion-core/redact/privacy-filter";
import type { RawEvent, Segment, SessionMetadata } from "@modelstat/core";
import { type LocalToolContext, resolveGitContext } from "@modelstat/parsers";
import { enrichToolCallScripts } from "./enrich-scripts.js";

let adapters: PipelineAdapters | null = null;

/** Builds the single set of pipeline adapters: a transformers.js
 * embedder, the bundled Qwen3.5-4B summariser + cognizer, a local
 * tokenizer heuristic, and the model-based PII redactor. Only the
 * SUMMARISER is mandatory — the others degrade gracefully (the redactor
 * falls back to the regex pass, cognition is best-effort).
 *
 * Cognition pass — best-effort. Uses the SAME bundled Qwen3.5-4B
 * model as the summariser via a second LlamaChatSession (separate
 * context sequence with `COGNITION_SYSTEM_PROMPT`). Adds ~200-500ms
 * per segment on Apple Silicon and a few tens of MB of KV-cache RAM
 * for the second context. The pipeline catches any failure and ships
 * the segment without the `[Mood: …] [Mind: …]` suffix, so wiring it
 * here is purely additive. */
async function bundledAdapters(): Promise<PipelineAdapters> {
  const llamaCfg = defaultLlamaConfig();
  return {
    // transformers.js BGE-small embedder — the wire embedding is
    // 384-dim (BGE-small), so segment vectors are directly comparable
    // across runtimes via cosine similarity. (This path used to ship
    // vector-less with empty arrays; hooking embeddings here attaches a
    // real abstract embedding to each segment.)
    embed: createTransformersJsEmbedder(),
    summarize: llamaSummarize(llamaCfg),
    tokenize: (text: string) => Math.max(1, Math.ceil(text.length / 4)),
    cognize: llamaCognize(llamaCfg),
    // Session-title pass — same bundled model, third chat session with
    // TITLER_SYSTEM_PROMPT. One short call per session per upload (the
    // sessions-list title in the dashboard). Best-effort like cognize:
    // failures fall back to a deterministic title in buildSessionTitles.
    entitle: llamaEntitle(llamaCfg),
    // Link-extraction pass — same bundled model, fifth chat session with
    // LINK_EXTRACT_SYSTEM_PROMPT. One short call per session that surfaces
    // PR/issue/commit references from the redacted abstracts, so detection
    // works even for clients whose logs carry no git data. Best-effort:
    // failures fall back to the deterministic git + content channels in
    // buildSessionMetadata.
    extractLinks: llamaExtractLinks(llamaCfg),
    // Model-based PII redactor (OpenAI Privacy Filter via
    // transformers.js / ONNX). Runs locally on CPU after the regex
    // pass in packages/core/redact.ts. ~1 GB model downloaded on
    // first run; subsequent runs reuse the cached weights. The
    // factory is async because it dynamic-imports
    // @huggingface/transformers — if the optional peer dep isn't
    // installed it returns a pass-through redactor (regex pass is
    // still the last line of defence).
    redact: await createPrivacyFilterRedactor(),
  };
}

async function getAdapters(): Promise<PipelineAdapters> {
  if (adapters) return adapters;
  // Single summariser path: the bundled node-llama-cpp runtime, staged
  // beside the bundle at install time (apps/daemon/src/service.ts
  // `installNativeRuntime`). Require the native binding to load; if it
  // can't, throw so the user sees the problem at daemon start rather than
  // discovering it three days later via a wall of garbage abstracts.
  try {
    await import("node-llama-cpp");
  } catch (err) {
    throw new Error(
      "modelstat daemon can't start: the bundled summariser (node-llama-cpp) failed to " +
        "load. Re-run `modelstat connect` (or `npm i -g modelstat`) so the native runtime " +
        `is re-staged beside the bundle. Underlying error: ${(err as Error).message}`,
    );
  }
  // biome-ignore lint/suspicious/noConsole: one-line startup status
  console.log("[modelstat] using bundled local summariser (Qwen3.5-4B, runs on this machine)");
  adapters = await bundledAdapters();
  return adapters;
}

export async function buildSegments(
  events: RawEvent[],
  onProgress?: SegmentProgressFn,
): Promise<Segment[]> {
  return buildSegmentsForSession(events, await getAdapters(), onProgress);
}

/**
 * One title per session from the given segments — the sessions-list
 * line in the dashboard. Runs the local titler (same bundled model,
 * see `entitle` above); deterministic fallback inside companion-core
 * means this never throws for healthy segments.
 */
export async function buildSessionTitles(segments: Segment[]): Promise<Record<string, string>> {
  const a = await getAdapters();
  return buildSessionTitlesCore(segments, a.entitle);
}

/**
 * Per-session repo/PR/commit/issue metadata for the given segments + events
 * — the join layer between AI spend and shipped work. Fuses git context
 * (resolved on disk via the parsers' `resolveGitContext`, which is cwd-cached
 * for the process), deterministic scanning of the redacted abstracts, and the
 * bundled link-extraction model (see `extractLinks` above). Best-effort
 * throughout: any channel can no-op without blocking the upload.
 */
export async function buildSessionMetadata(
  segments: Segment[],
  events: RawEvent[],
): Promise<Record<string, SessionMetadata>> {
  const a = await getAdapters();
  return buildSessionMetadataCore(segments, events, {
    resolveGit: resolveGitContext,
    extractLinks: a.extractLinks,
  });
}

/** Max bytes read from any one script before summarising. Scripts are small;
 * this bounds memory + model input (the prompt builder slices further). */
const MAX_SCRIPT_READ_BYTES = 64 * 1024;

let scriptSummarizer: ScriptSummarizer | null = null;

/**
 * Fill each draft's `ToolAction.scripts` with on-device, redacted per-script
 * content abstracts. Runs the bundled model (fourth chat session)
 * over the script/bash FILES a command referenced, reading them locally; only
 * the redacted one-sentence abstracts ship.
 *
 * Best-effort + additive: gated on the native runtime being loadable (same as
 * summarisation, via `getAdapters`); individual script failures are swallowed
 * inside `enrichToolCallScripts`. Mutates the drafts in place. No-op when there
 * are no shell contexts (e.g. an MCP-only file).
 */
export async function enrichScripts(
  drafts: readonly ToolCallDraft[],
  contexts: readonly LocalToolContext[] = [],
): Promise<void> {
  if (contexts.length === 0 || drafts.length === 0) return;
  // Gate on the native runtime being loadable — same requirement as the
  // summariser. (getAdapters is cached; usually already warm by this point.)
  await getAdapters();
  if (!scriptSummarizer) scriptSummarizer = llamaScriptSummarize(defaultLlamaConfig());
  await enrichToolCallScripts(drafts, contexts, {
    summarize: scriptSummarizer,
    exists: existsSync,
    readFile: async (path) => {
      const buf = await fsReadFile(path);
      return buf.subarray(0, MAX_SCRIPT_READ_BYTES).toString("utf8");
    },
  });
}

/**
 * Preflight check — exercise the summariser end-to-end at daemon
 * startup so a broken adapter (missing native binary, bad or truncated
 * model file) surfaces NOW instead of being noticed three days later
 * when the user opens the dashboard and sees a thousand "100 turns on
 * claude_code" abstracts.
 *
 * Returns the human-readable label of the active summariser. Throws
 * with an actionable message if anything is wrong. Called from
 * `modelstat start` / `scan` boot.
 */
export async function preflightSummariser(): Promise<string> {
  const a = await getAdapters();
  const out = await a.summarize({
    prompt:
      'Session context: smoke test. Sampled excerpts:\n  [turn 1] "hello world"\nWrite ONE sentence (≤240 chars) describing what the human was doing.',
    maxTokens: 32,
  });
  if (!out || out.trim().length === 0) {
    throw new Error(
      "summariser preflight returned empty output — the configured summariser " +
        "is reachable but produced no text. Check the model is loaded.",
    );
  }
  return out.length > 60 ? `${out.slice(0, 57)}…` : out;
}
