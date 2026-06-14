/**
 * CLI-side pipeline binding — wires the summariser/embedder pair the
 * agent runs and hands them to companion-core.
 *
 * ONE summariser path, no fallback: the bundled `node-llama-cpp`
 * runtime. The agent ships `node-llama-cpp` as a real dependency and
 * stages it (plus this platform's prebuilt binary) beside the installed
 * bundle at install time — see apps/agent-dev/src/service.ts
 * `installNativeRuntime`. A ~2.7 GB Qwen3.5-4B GGUF is lazy-downloaded
 * to `~/.modelstat/models/` on first scan. This runs on every supported
 * machine (macOS arm64/x64, Linux) with no external runtime to install,
 * configure, or keep alive — no Ollama, no separate daemon, no probing.
 *
 * **No silent no-op fallback.** Summarisation is core product output:
 * if the native runtime can't load on this machine the agent throws at
 * startup so the user sees the failure immediately, instead of happily
 * uploading thousands of useless "100 turns on claude_code" abstracts
 * that look fine in the database but tell them nothing.
 *
 * The adapters are built once and cached for the life of the process.
 */
import type { RawEvent, Segment } from "@modelstat/core";
import {
  buildSegmentsForSession,
  buildSessionTitles as buildSessionTitlesCore,
  type PipelineAdapters,
  type SegmentProgressFn,
} from "@modelstat/companion-core/pipeline";
import {
  createTransformersJsEmbedder,
  defaultLlamaConfig,
  llamaCognize,
  llamaEntitle,
  llamaSummarize,
} from "@modelstat/companion-core/node";
import { createPrivacyFilterRedactor } from "@modelstat/companion-core/redact/privacy-filter";

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
  // beside the bundle at install time (apps/agent-dev/src/service.ts
  // `installNativeRuntime`). Require the native binding to load; if it
  // can't, throw so the user sees the problem at agent start rather than
  // discovering it three days later via a wall of garbage abstracts.
  try {
    await import("node-llama-cpp");
  } catch (err) {
    throw new Error(
      "modelstat agent can't start: the bundled summariser (node-llama-cpp) failed to " +
        "load. Re-run `modelstat connect` (or `npm i -g modelstat`) so the native runtime " +
        `is re-staged beside the bundle. Underlying error: ${(err as Error).message}`,
    );
  }
  // biome-ignore lint/suspicious/noConsole: one-line startup status
  console.log(
    "[modelstat] using bundled local summariser (Qwen3.5-4B, runs on this machine)",
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
 * see `entitle` above); deterministic fallback inside companion-core
 * means this never throws for healthy segments.
 */
export async function buildSessionTitles(
  segments: Segment[],
): Promise<Record<string, string>> {
  const a = await getAdapters();
  return buildSessionTitlesCore(segments, a.entitle);
}

/**
 * Preflight check — exercise the summariser end-to-end at agent
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
      "Session context: smoke test. Sampled excerpts:\n  [turn 1] \"hello world\"\nWrite ONE sentence (≤240 chars) describing what the human was doing.",
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
