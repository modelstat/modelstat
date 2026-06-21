/**
 * CLI-side pipeline binding — wires the summariser/embedder pair the
 * daemon runs and hands them to daemon-core.
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
  llamaRedact,
  llamaScriptSummarize,
  llamaSummarize,
} from "@modelstat/daemon-core/node";
import {
  buildSegmentsForSession,
  buildSessionMetadata as buildSessionMetadataCore,
  buildSessionTitles as buildSessionTitlesCore,
  composeRedactors,
  heuristicSummarize,
  type PipelineAdapters,
  type Redactor,
  type ScriptSummarizer,
  type SegmentProgressFn,
  type Summarizer,
} from "@modelstat/daemon-core/pipeline";
import type { ToolCallDraft } from "@modelstat/daemon-core/queue";
import { createPrivacyFilterRedactor } from "@modelstat/daemon-core/redact/privacy-filter";
import type { RawEvent, Segment, SessionMetadata } from "@modelstat/core";
import { checkPullRequestOutcome, type LocalToolContext, resolveGitContext } from "@modelstat/parsers";
import { enrichToolCallRedaction, enrichToolCallScripts } from "./enrich-scripts.js";
import { runtimeState } from "./runtime-state.js";

let adapters: PipelineAdapters | null = null;

// ── Always-works summariser ────────────────────────────────────────────────
// The bundled Qwen LLM is the quality path, but it must NEVER block ingest. If
// it can't run on this machine (native runtime missing, model not downloaded,
// no network, incompatible binary, or even CPU load failing after the Metal
// guard), we degrade to the dependency-free extractive fallback so the daemon
// keeps shipping real abstracts instead of refusing to start.

let degradedThisProcess = false;
// After a CATCHABLE llm failure, skip the LLM for this long before retrying, so
// a persistent failure (model 404, offline, missing runtime) doesn't re-attempt
// — and potentially re-download the 2.7 GB GGUF — on every single segment.
const LLM_RETRY_COOLDOWN_MS = 10 * 60_000;
let llmRetryAfter = 0;

/** True if this process has fallen back to the extractive summariser at least
 * once — read by daemon.ts for the degraded status line + self-heal. */
export function summariserDegradedThisProcess(): boolean {
  return degradedThisProcess;
}

function markDegraded(reason: string): void {
  if (!degradedThisProcess) {
    degradedThisProcess = true;
    // biome-ignore lint/suspicious/noConsole: loud, one-time degradation notice
    console.warn(
      `[modelstat] ⚠ summariser DEGRADED — bundled LLM unavailable (${reason}); shipping ` +
        "extractive fallback abstracts so ingest continues. They re-summarise at model " +
        "quality automatically once the LLM is healthy again.",
    );
  }
  // Persist so the NEXT start can self-heal (re-scan to upgrade these abstracts).
  runtimeState.setSummariserDegraded(true);
}

/**
 * The daemon's summariser: the bundled Qwen LLM when it loads, the
 * dependency-free extractive fallback when it can't. Per-call fallback on a
 * catchable LLM failure, debounced so a persistent failure doesn't hammer the
 * LLM (or its model download). An UNCATCHABLE native abort (e.g. the Metal
 * GGML_ASSERT) is handled one layer down by the CPU-fallback guard in
 * daemon-core/node/llama.ts. Never throws, never empty — ingest always proceeds.
 */
function resilientSummarize(llamaCfg: Parameters<typeof llamaSummarize>[0]): Summarizer {
  const llm = llamaSummarize(llamaCfg);
  const heuristic = heuristicSummarize();
  return async (input) => {
    if (Date.now() >= llmRetryAfter) {
      try {
        const out = await llm(input);
        if (out && out.trim().length > 0) return out;
        // Empty LLM output — treat as a transient miss; use the fallback for
        // this one call without entering cooldown (the model is loaded/fine).
      } catch (err) {
        llmRetryAfter = Date.now() + LLM_RETRY_COOLDOWN_MS;
        markDegraded((err as Error).message);
      }
    } else {
      markDegraded("LLM in post-failure cooldown");
    }
    return heuristic(input);
  };
}

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
    summarize: resilientSummarize(llamaCfg),
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
    // Defense-in-depth redaction, layers 2+3, stacked behind one adapter and
    // applied to BOTH the abstract (in daemon-core) and `command_redacted` (in
    // enrichRedaction below): the OpenAI Privacy Filter (NER/PII) then the
    // local-LLM backstop for secrets the fixed patterns miss. Layer 1 (the
    // deterministic regex floor in @modelstat/core/redact) already ran first.
    redact: composeRedactors(await createPrivacyFilterRedactor(), llamaRedact(llamaCfg)),
  };
}

async function getAdapters(): Promise<PipelineAdapters> {
  if (adapters) return adapters;
  // The bundled node-llama-cpp summariser is the quality path, staged beside the
  // bundle at install time (apps/daemon/src/service.ts `installNativeRuntime`).
  // It must never BLOCK ingest, though: if the native binding can't load we
  // degrade to the dependency-free extractive fallback (resilientSummarize; the
  // auxiliary llama passes already no-op) rather than refusing to start and
  // leaving the user with zero data. Probe the import for an honest one-line
  // startup log — but do NOT throw on failure.
  let llmReady = true;
  try {
    await import("node-llama-cpp");
  } catch {
    llmReady = false;
  }
  // biome-ignore lint/suspicious/noConsole: one-line startup status
  console.log(
    llmReady
      ? "[modelstat] using bundled local summariser (Qwen3.5-4B, runs on this machine)"
      : "[modelstat] bundled summariser runtime not loadable — using extractive fallback (degraded) until it is",
  );
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
 * see `entitle` above); deterministic fallback inside daemon-core
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
    checkPrOutcome: checkPullRequestOutcome,
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
  if (drafts.length === 0) return;
  // Gate on the native runtime being loadable — same requirement as the
  // summariser. (getAdapters is cached; usually already warm by this point.)
  const built = await getAdapters();
  // Defense-in-depth over `command_redacted` for EVERY draft (not just ones that
  // ran a script): layers 2+3 (Privacy Filter + LLM backstop) on top of the
  // layer-1 floor that already ran at extraction.
  if (built.redact) await enrichToolCallRedaction(drafts, built.redact);
  // Per-script content summaries only apply when a command ran a script FILE.
  if (contexts.length === 0) return;
  if (!scriptSummarizer) scriptSummarizer = llamaScriptSummarize(defaultLlamaConfig());
  await enrichToolCallScripts(drafts, contexts, {
    summarize: scriptSummarizer,
    exists: existsSync,
    readFile: async (path) => {
      const buf = await fsReadFile(path);
      return buf.subarray(0, MAX_SCRIPT_READ_BYTES).toString("utf8");
    },
    modelRedact: built.redact,
  });
}

/**
 * Preflight check — exercise the summariser end-to-end at daemon
 * startup so its state (real LLM vs degraded extractive fallback)
 * surfaces NOW, in the startup log, instead of being inferred later
 * from abstract quality.
 *
 * Returns the active summariser's label and whether it's running
 * DEGRADED (the LLM couldn't load, extractive fallback in use). Never
 * throws — the fallback is deterministic, so ingest always proceeds.
 * Called from `modelstat start` / `scan` boot.
 */
export async function preflightSummariser(): Promise<{ label: string; degraded: boolean }> {
  const a = await getAdapters();
  const out = await a.summarize({
    prompt:
      'Session context: smoke test. Sampled excerpts:\n  [turn 1] "hello world"\nWrite ONE sentence (≤240 chars) describing what the human was doing.',
    maxTokens: 32,
    excerpts: ["smoke test — verifying the summariser is alive"],
    facts: "preflight smoke test",
  });
  const degraded = summariserDegradedThisProcess();
  // The fallback is deterministic + never empty, so empty output means even it
  // failed — degrade rather than throw (ingest availability wins).
  if (!out || out.trim().length === 0) {
    return { label: "summariser produced no output", degraded: true };
  }
  const sample = out.length > 60 ? `${out.slice(0, 57)}…` : out;
  const engine = degraded ? "extractive fallback (LLM unavailable)" : "Qwen3.5-4B";
  return { label: `${engine} — "${sample}"`, degraded };
}
