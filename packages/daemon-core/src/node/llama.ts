/**
 * Bundled local-LLM summariser for the Node CLI — runs a local Qwen3
 * GGUF via `node-llama-cpp` so the agent produces real content
 * abstracts even when the user has no Ollama installed.
 *
 *   model       : Qwen3.5-4B-Q4_K_M.gguf (~2.7 GB, 4-bit quantised) — the
 *                 most capable Qwen that ships to a laptop (~2 GB class);
 *                 lands near Qwen2.5-32B on summarisation.
 *   download    : lazy on first call → cached at ~/.modelstat/models/
 *   inference   : ~0.5–3 s / segment (Metal/CPU-dependent; 4B is heavier
 *                 than the old 0.6B but far higher quality)
 *   contract    : same Summarizer signature as ollamaSummarize
 *
 * `node-llama-cpp` ships prebuilt native binaries per platform via npm,
 * so we declare it as an optional peerDependency here and a real
 * dependency in the agent CLI. Imports are dynamic so a daemon that
 * doesn't have the package installed (e.g. the bundled service when
 * node_modules isn't beside the bundle) silently falls through to the
 * next adapter in the chain — see apps/daemon/src/pipeline.ts.
 */

import { existsSync, rmSync, writeFileSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import {
  buildCognitionUserPrompt,
  COGNITION_MAX_TOKENS,
  COGNITION_SYSTEM_PROMPT,
  COGNITION_TEMPERATURE,
  type Cognizer,
  parseCognitionReply,
} from "../pipeline/cognition.js";
import type { Redactor } from "../pipeline/index.js";
import { SUMMARISER_SYSTEM_PROMPT, SUMMARISER_TEMPERATURE } from "../pipeline/prompts.js";
import {
  applyLlmRedactions,
  parseRedactReply,
  REDACT_MAX_TOKENS,
  REDACT_SYSTEM_PROMPT,
  REDACT_TEMPERATURE,
  shouldDeepRedact,
} from "../pipeline/redaction.js";
import {
  buildScriptSummaryUserPrompt,
  SCRIPT_SUMMARY_MAX_TOKENS,
  SCRIPT_SUMMARY_OUTPUT_MAX_CHARS,
  SCRIPT_SUMMARY_SYSTEM_PROMPT,
  SCRIPT_SUMMARY_TEMPERATURE,
  type ScriptSummarizer,
} from "../pipeline/script-summary.js";
import {
  buildLinkExtractUserPrompt,
  LINK_EXTRACT_MAX_TOKENS,
  LINK_EXTRACT_SYSTEM_PROMPT,
  LINK_EXTRACT_TEMPERATURE,
  type LinkExtractor,
} from "../pipeline/session-metadata.js";
import { stripThinking, THINKING_HEADROOM_TOKENS } from "../pipeline/thinking.js";
import {
  buildTitleUserPrompt,
  type Entitler,
  TITLER_MAX_TOKENS,
  TITLER_SYSTEM_PROMPT,
  TITLER_TEMPERATURE,
} from "../pipeline/title.js";

export interface LlamaConfig {
  /** Filesystem path to a GGUF model. If absent, we download `modelUrl`
   * to `<modelsDir>/<basename(modelUrl)>` on first use. */
  modelPath?: string;
  /** Remote URL to download the GGUF from when `modelPath` isn't set
   * or doesn't exist on disk. */
  modelUrl?: string;
  /** Where to cache downloaded models. Default: `~/.modelstat/models`. */
  modelsDir?: string;
  /** Llama context window — kept small since prompts are bounded. */
  contextSize?: number;
}

/**
 * Default bundled-summariser model — Qwen3.5-4B Q4_K_M GGUF (~2.7 GB).
 *
 * Selection rationale (April 2026):
 *   - Latest dense Qwen at the size we can ship to a laptop. Qwen3.6
 *     only exists at 27B+ which is too large.
 *   - Hybrid-thinking: the model writes `<think>…</think>` reasoning
 *     before the answer, which materially improves "describe what the
 *     human was building" outputs over non-thinking small models. The
 *     adapter strips the thinking block before returning.
 *   - Q4_K_M strikes the right quality/size balance for a one-shot
 *     ~240-char summary task; smaller quants (Q3) start fumbling
 *     instruction-following at this size.
 *   - lmstudio-community is the trusted re-quantiser used by LM Studio
 *     itself, so the URL is stable and the templates baked into the
 *     GGUF metadata are correct (node-llama-cpp auto-detects them).
 *
 * The first scan on a fresh machine downloads ~2.7 GB; subsequent
 * scans are instant. Override with `MODELSTAT_LLAMA_MODEL_URL` (and
 * `MODELSTAT_LLAMA_MODEL_PATH`) to point at a different GGUF — useful
 * for testing or for users who already have a quant on disk.
 */
export const DEFAULT_LLAMA_MODEL_URL =
  "https://huggingface.co/lmstudio-community/Qwen3.5-4B-GGUF/resolve/main/Qwen3.5-4B-Q4_K_M.gguf";

/** Token budget for one summarise call. The Qwen3.5 thinking pass
 * routinely uses 400-800 tokens before producing the answer; ceil at
 * 1024 so we have ~200 tokens left for the actual sentence. The
 * pipeline still slices the post-`<think>` text to 240 chars. */
const LLAMA_MAX_TOKENS = 1024;

export function defaultLlamaConfig(): Required<LlamaConfig> {
  const env =
    (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env ?? {};
  const modelsDir = env.MODELSTAT_MODELS_DIR ?? join(homedir(), ".modelstat", "models");
  const modelUrl = env.MODELSTAT_LLAMA_MODEL_URL ?? DEFAULT_LLAMA_MODEL_URL;
  const modelPath = env.MODELSTAT_LLAMA_MODEL_PATH ?? join(modelsDir, basenameFromUrl(modelUrl));
  return {
    modelPath,
    modelUrl,
    modelsDir,
    // Qwen3.5-4B has a 128K native context; we only need enough to
    // fit our prompt (~2 KB) plus thinking budget plus answer.
    // 4096 is plenty and keeps memory footprint reasonable on
    // older machines.
    contextSize: Number(env.MODELSTAT_LLAMA_CONTEXT ?? 4096),
  };
}

function basenameFromUrl(url: string): string {
  // Strip query/fragment and grab the trailing filename.
  const clean = url.split("?")[0]!.split("#")[0]!;
  const parts = clean.split("/");
  return parts[parts.length - 1] || "model.gguf";
}

/* ─── Singleton model + serialised inference ─────────────────────── */

// Loading is expensive (model file mmap + warm-up): hold onto the
// sessions for the life of the process. Inference is single-threaded
// per llama context, so callers are queued through `inflight`.
//
// Four LlamaChatSessions share the same loaded model but each has its
// own context sequence with a different system prompt:
//   - summarizer:      SUMMARISER_SYSTEM_PROMPT     — primary scan-time work
//   - cognizer:        COGNITION_SYSTEM_PROMPT      — best-effort mood/mode pass
//   - entitler:        TITLER_SYSTEM_PROMPT         — best-effort per-session title
//   - scriptSummarizer: SCRIPT_SUMMARY_SYSTEM_PROMPT — best-effort per-script content abstract
//   - linkExtractor:    LINK_EXTRACT_SYSTEM_PROMPT   — best-effort per-session PR/issue/commit refs
// Each context's KV-cache is small (≤4K tokens) compared to the model
// weights (~2.7GB), so the extra contexts are cheap. We still serialise
// all calls through one `inflight` queue — llama.cpp can technically
// run several contexts in parallel but it competes for the same CPU/GPU
// and the auxiliary passes are fast enough that interleaving isn't
// worth the complexity.
type Session = {
  // Kept loosely typed — actual class shapes come from a dynamic import
  // so we don't drag node-llama-cpp's types into this file.
  prompt: (p: string, o: unknown) => Promise<string>;
  resetChatHistory: () => void;
};
type Loaded = {
  summarizer: Session;
  cognizer: Session;
  entitler: Session;
  scriptSummarizer: Session;
  linkExtractor: Session;
  redactor: Session;
};
let loaded: Loaded | null = null;
let loadPromise: Promise<Loaded> | null = null;
let inflight: Promise<unknown> = Promise.resolve();
// The underlying node-llama-cpp instance, kept so we can dispose it (and
// every model/context spun off it) before the process exits — see
// disposeLlama() for why that matters on macOS/Metal.
let llamaInstance: { dispose: () => Promise<void> } | null = null;

/**
 * Tear the bundled summariser down cleanly before the process exits.
 *
 * node-llama-cpp's llama.cpp Metal backend aborts with
 * `GGML_ASSERT([rsets->data count] == 0)` when its device is freed by C++
 * static destructors at `exit()` while contexts are still alive (macOS).
 * The agent hit this on every launchd stop/restart: the daemon's `exit(0)`
 * crashed instead of exiting clean, so launchd saw a failed exit. Disposing
 * the Llama instance here frees its contexts + model + device in order, so
 * the static-destructor path has nothing left to free.
 *
 * No-op when the bundled summariser was never loaded (e.g. the Ollama
 * path, or a process that never summarised), so it is always safe to call
 * from a shutdown handler / before a one-shot command returns.
 */
export async function disposeLlama(): Promise<void> {
  const inst = llamaInstance;
  llamaInstance = null;
  loaded = null;
  loadPromise = null;
  if (!inst) return;
  try {
    await inst.dispose();
  } catch {
    // Best-effort: if dispose itself throws we still want a clean process
    // exit; the OS reclaims the resources regardless.
  }
}

/**
 * Download the summariser GGUF to disk if it isn't already there.
 * Exported so the agent CLI can call it in two places:
 *   1. npm `postinstall` — pull the model right after `npm install`
 *      so the user sees the long download attached to the install
 *      they just kicked off, not as a surprise mid-`scan`.
 *   2. `npx modelstat@latest` step 2.5 — guarantee the model is on
 *      disk BEFORE installing the background service, so the
 *      service's first preflight succeeds and real abstracts start
 *      streaming immediately rather than after a silent 2.7 GB pull.
 *
 * Idempotent: returns the path immediately if the file exists.
 *
 * Progress is rendered as either a single redrawing TTY line (when
 * stdout is a TTY) or as periodic newline-separated lines (in pipes
 * / log files) so postinstall output reads cleanly in both contexts.
 */
export async function ensureLlamaModel(
  cfg: Required<LlamaConfig> = defaultLlamaConfig(),
): Promise<string> {
  if (existsSync(cfg.modelPath)) return cfg.modelPath;
  await mkdir(dirname(cfg.modelPath), { recursive: true });

  const res = await fetch(cfg.modelUrl);
  if (!res.ok || !res.body) {
    throw new Error(`model download failed: ${res.status} ${res.statusText} (${cfg.modelUrl})`);
  }
  const total = Number(res.headers.get("content-length") ?? 0);
  const totalMb = total > 0 ? (total / (1024 * 1024)).toFixed(0) : "?";
  // biome-ignore lint/suspicious/noConsole: long download — surface it
  console.log(`[modelstat] downloading summariser model (~${totalMb} MB) → ${cfg.modelPath}`);
  const tmp = `${cfg.modelPath}.partial`;
  const { createWriteStream } = await import("node:fs");
  const { Readable } = await import("node:stream");
  const { rename } = await import("node:fs/promises");
  const out = createWriteStream(tmp);
  const isTty = Boolean((process.stdout as unknown as { isTTY?: boolean }).isTTY);
  let received = 0;
  let lastLog = 0;
  let lastBytes = 0;
  let lastTimeForRate = Date.now();
  const startTime = Date.now();
  const renderProgress = (final = false) => {
    const now = Date.now();
    const dt = (now - lastTimeForRate) / 1000;
    const dBytes = received - lastBytes;
    const rateMbps = dt > 0 ? dBytes / 1024 / 1024 / dt : 0;
    lastBytes = received;
    lastTimeForRate = now;
    const mb = (received / (1024 * 1024)).toFixed(1);
    const pct = total > 0 ? ((received / total) * 100).toFixed(1) : "?";
    const eta =
      total > 0 && rateMbps > 0
        ? Math.max(0, Math.round((total - received) / 1024 / 1024 / rateMbps))
        : null;
    const etaStr = eta != null ? ` · ETA ${eta}s` : "";
    const elapsed = ((now - startTime) / 1000).toFixed(0);
    const line = `[modelstat]   ${mb} / ${totalMb} MB (${pct}%) · ${rateMbps.toFixed(1)} MB/s${etaStr} · ${elapsed}s`;
    if (isTty && !final) {
      process.stdout.write(`\r${line}\x1b[K`);
    } else {
      // biome-ignore lint/suspicious/noConsole: progress
      console.log(line);
    }
  };

  const nodeStream = Readable.fromWeb(
    res.body as unknown as Parameters<typeof Readable.fromWeb>[0],
  );
  nodeStream.on("data", (chunk: Buffer) => {
    received += chunk.length;
    const now = Date.now();
    if (now - lastLog > (isTty ? 200 : 2000)) {
      renderProgress();
      lastLog = now;
    }
  });
  await new Promise<void>((resolve, reject) => {
    nodeStream.pipe(out);
    out.on("finish", () => resolve());
    out.on("error", reject);
    nodeStream.on("error", reject);
  });
  renderProgress(true);
  if (isTty) process.stdout.write("\n");
  await rename(tmp, cfg.modelPath);
  // biome-ignore lint/suspicious/noConsole: completion
  console.log(`[modelstat] summariser model ready (${cfg.modelPath})`);
  return cfg.modelPath;
}

async function loadOnce(cfg: Required<LlamaConfig>): Promise<Loaded> {
  if (loaded) return loaded;
  if (loadPromise) return loadPromise;
  loadPromise = (async () => {
    // Dynamic import so the rest of daemon-core stays usable when
    // `node-llama-cpp` isn't installed (browser, server, slimmed
    // bundles). Failure here is the caller's signal to fall through.
    // Optional peerDependency — typed as `unknown` from this side so
    // daemon-core typechecks even when the consumer hasn't pulled
    // the package in. Agent CLI declares `node-llama-cpp` as a real
    // dep so the import resolves at runtime there.
    // @ts-expect-error — optional peer; resolved at runtime
    const llamaMod = (await import("node-llama-cpp")) as {
      getLlama: (opts?: { gpu?: false }) => Promise<unknown>;
      LlamaChatSession: new (opts: unknown) => Session;
    };
    const modelPath = await ensureLlamaModel(cfg);
    // Metal-abort guard. On some Macs node-llama-cpp's llama.cpp Metal backend
    // ABORTS the whole process during init/load with `GGML_ASSERT([rsets->data
    // count] == 0)` ("the tensor API is not supported in this environment") — an
    // uncatchable C++ abort, so try/catch can't save us. Instead we arm a guard
    // file before touching Metal and disarm it once the model has loaded: a file
    // left behind means the last start aborted on Metal, so this start runs on
    // CPU. Working-Metal Macs keep Metal (fast); a broken-Metal Mac summarises on
    // CPU instead of crash-looping. Delete the guard to re-probe Metal (e.g.
    // after a node-llama-cpp upgrade).
    const guardPath = `${dirname(modelPath)}/.metal-load-guard`;
    const metalAborted = existsSync(guardPath);
    if (!metalAborted) {
      try {
        writeFileSync(guardPath, "probing metal\n");
      } catch {
        /* best-effort guard; proceed regardless */
      }
    }
    const llama = (await llamaMod.getLlama(metalAborted ? { gpu: false } : undefined)) as {
      loadModel: (o: { modelPath: string }) => Promise<{
        createContext: (o: { contextSize: number }) => Promise<{
          getSequence: () => unknown;
        }>;
      }>;
      dispose: () => Promise<void>;
    };
    // Hold the instance so disposeLlama() can free it before the process
    // exits (clean Metal teardown).
    llamaInstance = llama;
    const model = await llama.loadModel({ modelPath });
    // Disarm the guard ONLY when we just proved METAL works (we probed it this
    // start — i.e. the guard wasn't already set). A successful CPU load must NOT
    // remove the "Metal is broken on this machine" marker: otherwise the next
    // clean start re-probes Metal and aborts again, OSCILLATING crash↔CPU on
    // every restart (a broken-Metal Mac never settles on CPU — exactly the
    // crash-loop we hit). The guard now STICKS until a node-llama-cpp upgrade
    // (or a manual delete) re-probes Metal.
    if (!metalAborted) {
      try {
        rmSync(guardPath, { force: true });
      } catch {
        /* best-effort */
      }
    }
    // Two contexts off the same model — one per chat session. The
    // cognition context can be smaller since its prompt + answer are
    // both short, but llama.cpp rounds context sizes up to its block
    // size internally so going below ~1024 saves nothing meaningful.
    const summariserContext = await model.createContext({
      contextSize: cfg.contextSize,
    });
    const cognizerContext = await model.createContext({
      contextSize: Math.min(cfg.contextSize, 2048),
    });
    // Title prompts carry up to ~10 sampled abstracts (~2.5 KB) plus
    // the thinking budget — the same envelope as the summariser, so
    // reuse the full configured context size rather than the small one.
    const entitlerContext = await model.createContext({
      contextSize: cfg.contextSize,
    });
    // Script-summary prompts carry a (capped) script file body plus the
    // thinking budget — same envelope as the summariser.
    const scriptContext = await model.createContext({
      contextSize: cfg.contextSize,
    });
    const summarizer = new llamaMod.LlamaChatSession({
      contextSequence: summariserContext.getSequence(),
      systemPrompt: SUMMARISER_SYSTEM_PROMPT,
    });
    const cognizer = new llamaMod.LlamaChatSession({
      contextSequence: cognizerContext.getSequence(),
      systemPrompt: COGNITION_SYSTEM_PROMPT,
    });
    const entitler = new llamaMod.LlamaChatSession({
      contextSequence: entitlerContext.getSequence(),
      systemPrompt: TITLER_SYSTEM_PROMPT,
    });
    const scriptSummarizer = new llamaMod.LlamaChatSession({
      contextSequence: scriptContext.getSequence(),
      systemPrompt: SCRIPT_SUMMARY_SYSTEM_PROMPT,
    });
    // Link-extraction prompts carry up to ~12 sampled abstracts — same
    // envelope as the titler, so reuse the full configured context size.
    const linkExtractContext = await model.createContext({
      contextSize: cfg.contextSize,
    });
    const linkExtractor = new llamaMod.LlamaChatSession({
      contextSequence: linkExtractContext.getSequence(),
      systemPrompt: LINK_EXTRACT_SYSTEM_PROMPT,
    });
    // Redaction backstop — one short command in, a few candidate substrings out.
    const redactorContext = await model.createContext({
      contextSize: Math.min(cfg.contextSize, 2048),
    });
    const redactor = new llamaMod.LlamaChatSession({
      contextSequence: redactorContext.getSequence(),
      systemPrompt: REDACT_SYSTEM_PROMPT,
    });
    loaded = { summarizer, cognizer, entitler, scriptSummarizer, linkExtractor, redactor };
    return loaded;
  })();
  try {
    return await loadPromise;
  } catch (err) {
    loadPromise = null;
    throw err;
  }
}

/**
 * Summariser adapter — drop-in for `ollamaSummarize`. Same prompt
 * contract, same output cap (≤240 chars). On first call, pulls the
 * GGUF model to disk and warms up the llama context; subsequent calls
 * are queued through a single in-process serialiser.
 */
export function llamaSummarize(
  cfg: Required<LlamaConfig> = defaultLlamaConfig(),
): (input: { prompt: string; maxTokens: number }) => Promise<string> {
  return async ({ prompt, maxTokens }) => {
    const { summarizer } = await loadOnce(cfg);
    // Serialise: the chat session has one context sequence and
    // concurrent prompts would interleave. Replace `inflight` with
    // this call's promise so the next caller awaits us, not the one
    // before us.
    const run = inflight.then(async () => {
      summarizer.resetChatHistory();
      // Ignore the caller's tiny `maxTokens` (≈80) when running the
      // bundled thinking model — it'd starve the reasoning pass and
      // we'd ship empty abstracts. The sentence cap is enforced by
      // the post-strip slice below, not the token budget.
      void maxTokens;
      const raw = await summarizer.prompt(prompt, {
        temperature: SUMMARISER_TEMPERATURE,
        maxTokens: LLAMA_MAX_TOKENS,
      });
      const stripped = stripThinking(raw ?? "");
      if (stripped.length === 0) {
        // Either the model produced only thinking and ran out of
        // budget, or the chat template isn't recognised and we got
        // garbage. Either way the caller (daemon-core/pipeline)
        // throws on empty — surface a clearer reason here so the
        // operator can act on it.
        throw new Error(
          `bundled summariser produced no answer text after stripping <think> blocks (raw length=${(raw ?? "").length}). The thinking budget may be too low or the model template is misconfigured.`,
        );
      }
      return stripped.slice(0, 240);
    });
    inflight = run.catch(() => undefined);
    return run;
  };
}

/**
 * Cognition adapter — drop-in for `ollamaCognize`. Reads the post-
 * redaction abstract and tags `{ emotions, meta }` via the same Qwen
 * model that did the summarise pass, but on a separate
 * `LlamaChatSession` whose system prompt is `COGNITION_SYSTEM_PROMPT`.
 *
 * Best-effort: any failure (model not loaded, JSON parse fail, empty
 * answer after stripping <think>) returns null and the segment ships
 * without a `[Mood: …] [Mind: …]` suffix. The pipeline catches null
 * and continues — see `daemon-core/pipeline/index.ts` for the
 * append logic.
 *
 * Cost: one extra model call per segment, serialised through the same
 * `inflight` queue as the summariser. Typical latency on Apple Silicon
 * is 200-500 ms because the answer is tiny (~30 tokens of JSON) and
 * the thinking budget is capped low. The model itself is already
 * resident in memory, so the marginal RAM cost is just the second
 * 2K-token KV-cache (~tens of MB).
 */
export function llamaCognize(cfg: Required<LlamaConfig> = defaultLlamaConfig()): Cognizer {
  return async ({ abstract }) => {
    if (!abstract || abstract.trim().length < 12) return null;
    let loadedSessions: Loaded;
    try {
      loadedSessions = await loadOnce(cfg);
    } catch {
      // If the model isn't loadable here, the summariser would have
      // raised already and the agent wouldn't be running. Still:
      // best-effort by contract — fall through to null rather than
      // throwing during a post-summarise hook.
      return null;
    }
    const { cognizer } = loadedSessions;
    const run = inflight.then(async () => {
      cognizer.resetChatHistory();
      const raw = await cognizer.prompt(buildCognitionUserPrompt(abstract), {
        temperature: COGNITION_TEMPERATURE,
        // Qwen3.5 likes to "think" before answering. Give it a small
        // budget — the JSON answer is ~30 tokens but the thinking can
        // run 200-400. The strip below removes the <think> block.
        maxTokens: COGNITION_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      const stripped = stripThinking(raw ?? "");
      return parseCognitionReply(stripped);
    });
    inflight = run.catch(() => undefined);
    try {
      return await run;
    } catch {
      return null;
    }
  };
}

/**
 * Session-title adapter — one short call per session per upload, on a
 * third `LlamaChatSession` whose system prompt is `TITLER_SYSTEM_PROMPT`.
 * Reads the session's segment abstracts (already produced + redacted by
 * the summarise pass) and names the session's dominant theme(s).
 *
 * Best-effort, same contract as `llamaCognize`: any failure returns
 * null and the caller (`buildSessionTitles`) falls back to its
 * deterministic title. The model is already resident, so the marginal
 * cost is one ~40-token answer plus thinking budget — well under a
 * second per session on Apple Silicon.
 */
export function llamaEntitle(cfg: Required<LlamaConfig> = defaultLlamaConfig()): Entitler {
  return async (input) => {
    if (input.abstracts.length === 0) return null;
    let loadedSessions: Loaded;
    try {
      loadedSessions = await loadOnce(cfg);
    } catch {
      // Best-effort by contract — if the model can't load here the
      // summariser already surfaced the failure at startup.
      return null;
    }
    const { entitler } = loadedSessions;
    const run = inflight.then(async () => {
      entitler.resetChatHistory();
      const raw = await entitler.prompt(buildTitleUserPrompt(input), {
        temperature: TITLER_TEMPERATURE,
        // Same thinking-budget rationale as the cognition pass: the
        // answer is tiny but Qwen3.5 reasons first.
        maxTokens: TITLER_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      return stripThinking(raw ?? "") || null;
    });
    inflight = run.catch(() => undefined);
    try {
      return await run;
    } catch {
      return null;
    }
  };
}

/**
 * Per-script content-summary adapter — the fourth chat session, system prompt
 * `SCRIPT_SUMMARY_SYSTEM_PROMPT`. Reads a script/bash FILE's (capped) contents
 * and returns one factual sentence (≤200 chars) describing what running it does,
 * so the backend understands a command's real effect without seeing the file.
 *
 * Best-effort, same contract as `llamaCognize`/`llamaEntitle`: any failure
 * (model not loadable, empty answer after stripping <think>) returns null and
 * the agent ships that script without an abstract — the call still carries its
 * redacted command. The agent redacts the returned sentence before it goes on
 * the wire; the file contents themselves never leave the device.
 */
export function llamaScriptSummarize(
  cfg: Required<LlamaConfig> = defaultLlamaConfig(),
): ScriptSummarizer {
  return async ({ ref, content }) => {
    if (!content || content.trim().length === 0) return null;
    let loadedSessions: Loaded;
    try {
      loadedSessions = await loadOnce(cfg);
    } catch {
      // Best-effort by contract — if the model can't load here the
      // summariser already surfaced the failure at startup.
      return null;
    }
    const { scriptSummarizer } = loadedSessions;
    const run = inflight.then(async () => {
      scriptSummarizer.resetChatHistory();
      const raw = await scriptSummarizer.prompt(buildScriptSummaryUserPrompt({ ref, content }), {
        temperature: SCRIPT_SUMMARY_TEMPERATURE,
        // Qwen3.5 reasons before answering — give it room on top of the
        // one-sentence answer budget; the slice below enforces the cap.
        maxTokens: SCRIPT_SUMMARY_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      const oneLine = stripThinking(raw ?? "")
        .replace(/\s+/g, " ")
        .trim();
      return oneLine ? oneLine.slice(0, SCRIPT_SUMMARY_OUTPUT_MAX_CHARS) : null;
    });
    inflight = run.catch(() => undefined);
    try {
      return await run;
    } catch {
      return null;
    }
  };
}

/**
 * Link-extraction adapter — one short call per session, on a fifth
 * `LlamaChatSession` whose system prompt is `LINK_EXTRACT_SYSTEM_PROMPT`.
 * Reads the session's redacted abstracts and emits any PR/issue/commit/repo
 * references it sees as plain text (one per line, or `none`). The caller
 * (`buildSessionMetadata`) re-parses that text deterministically, so a
 * hallucinated line that isn't a valid reference is simply dropped.
 *
 * This is the provider-agnostic channel: it works for clients whose logs
 * carry no structured git data (web chat, Cursor), since it reads the same
 * summarised text the dashboard shows. Best-effort, same contract as
 * `llamaEntitle`: any failure returns null and detection falls back to the
 * deterministic git + content channels.
 */
export function llamaExtractLinks(
  cfg: Required<LlamaConfig> = defaultLlamaConfig(),
): LinkExtractor {
  return async ({ abstracts }) => {
    if (abstracts.length === 0) return null;
    let loadedSessions: Loaded;
    try {
      loadedSessions = await loadOnce(cfg);
    } catch {
      return null;
    }
    const { linkExtractor } = loadedSessions;
    const run = inflight.then(async () => {
      linkExtractor.resetChatHistory();
      const raw = await linkExtractor.prompt(buildLinkExtractUserPrompt(abstracts), {
        temperature: LINK_EXTRACT_TEMPERATURE,
        // Same thinking-budget rationale as cognition/title: the answer is a
        // few short lines but Qwen3.5 reasons first.
        maxTokens: LINK_EXTRACT_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      return stripThinking(raw ?? "") || null;
    });
    inflight = run.catch(() => undefined);
    try {
      return await run;
    } catch {
      return null;
    }
  };
}

/**
 * Local-LLM redaction backstop (layer 3) — see `../pipeline/redaction.ts`. Asks
 * the bundled model to name any secret substrings still present after the regex +
 * Privacy-Filter passes, then deletes those exact substrings deterministically.
 * Cheap pre-filter (`shouldDeepRedact`) skips the common no-secret command, so
 * most calls cost nothing. Fail-safe: model unavailable / error / empty reply →
 * the input is returned unchanged (the earlier layers already redacted it).
 */
export function llamaRedact(cfg: Required<LlamaConfig> = defaultLlamaConfig()): Redactor {
  return async (text: string) => {
    const unchanged = { text, counts: {} as Record<string, number> };
    if (!shouldDeepRedact(text)) return unchanged;
    let loadedSessions: Loaded;
    try {
      loadedSessions = await loadOnce(cfg);
    } catch {
      return unchanged;
    }
    const { redactor } = loadedSessions;
    const run = inflight.then(async () => {
      redactor.resetChatHistory();
      const raw = await redactor.prompt(text, {
        temperature: REDACT_TEMPERATURE,
        // Thinking budget on top of the short list of substrings.
        maxTokens: REDACT_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
      });
      return stripThinking(raw ?? "");
    });
    inflight = run.catch(() => undefined);
    let reply: string;
    try {
      reply = await run;
    } catch {
      return unchanged;
    }
    const candidates = parseRedactReply(reply);
    if (candidates.length === 0) return unchanged;
    const { text: redacted, count } = applyLlmRedactions(text, candidates);
    return { text: redacted, counts: count > 0 ? { llm_secrets: count } : {} };
  };
}
