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
 * next adapter in the chain.
 */

import { existsSync, rmSync, writeFileSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { COGNITION_SYSTEM_PROMPT, type Cognizer } from "../pipeline/cognition.js";
import type { Redactor, Summarizer } from "../pipeline/index.js";
import {
  type ChatComplete,
  makeCognizer,
  makeEntitler,
  makeLinkExtractor,
  makeScriptSummarizer,
  makeSummarizer,
} from "../pipeline/passes.js";
import { SUMMARISER_SYSTEM_PROMPT } from "../pipeline/prompts.js";
import {
  applyLlmRedactions,
  parseRedactReply,
  REDACT_MAX_TOKENS,
  REDACT_SYSTEM_PROMPT,
  REDACT_TEMPERATURE,
  shouldDeepRedact,
} from "../pipeline/redaction.js";
import { SCRIPT_SUMMARY_SYSTEM_PROMPT, type ScriptSummarizer } from "../pipeline/script-summary.js";
import { LINK_EXTRACT_SYSTEM_PROMPT, type LinkExtractor } from "../pipeline/session-metadata.js";
import { stripThinking, THINKING_HEADROOM_TOKENS } from "../pipeline/thinking.js";
import { type Entitler, TITLER_SYSTEM_PROMPT } from "../pipeline/title.js";

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
// Set once disposeLlama() begins teardown. Once armed, loadOnce() and the
// per-pass adapters REFUSE to admit new inference so we don't keep a context
// busy while we're trying to free the Metal device underneath it — the exact
// race that fired `libc++abi: terminating … mutex lock failed` on auto-update
// (the updater calls disposeLlama() while a scan segment was still in-flight).
let disposing = false;
// Hard cap on how long disposeLlama() waits for in-flight inference to drain
// before it frees the device anyway. A wedged prompt must not block process
// exit forever; 8s comfortably exceeds a single ≤4K-token completion.
const DISPOSE_DRAIN_TIMEOUT_MS = 8_000;

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
 *
 * Drains BEFORE freeing: `inst.dispose()` tears down the contexts + model +
 * Metal device, and llama.cpp aborts (`libc++abi … mutex lock failed`, or the
 * Metal `GGML_ASSERT`) if a context is mid-prompt when its device is freed.
 * The updater/shutdown path calls us while a scan segment can still be running,
 * so we (1) arm `disposing` to stop NEW work being admitted, then (2) await the
 * current `inflight` chain to settle — capped at {@link DISPOSE_DRAIN_TIMEOUT_MS}
 * so a wedged prompt can't block process exit forever — and only then dispose.
 */
export async function disposeLlama(): Promise<void> {
  disposing = true;
  const inst = llamaInstance;
  llamaInstance = null;
  loaded = null;
  loadPromise = null;
  if (!inst) return;
  // Drain whatever inference is in flight so no context is mid-prompt when we
  // free the device. `inflight` is the serialiser tail every adapter chains
  // onto; awaiting it (errors swallowed) means the last queued prompt has
  // returned. Race it against a hard timeout so a stuck prompt still exits.
  try {
    let drainTimer: ReturnType<typeof setTimeout> | undefined;
    const drainCap = new Promise<void>((resolve) => {
      drainTimer = setTimeout(resolve, DISPOSE_DRAIN_TIMEOUT_MS);
      drainTimer.unref?.();
    });
    await Promise.race([inflight.catch(() => undefined), drainCap]);
    if (drainTimer) clearTimeout(drainTimer);
  } catch {
    /* best-effort drain — fall through to dispose regardless */
  }
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
  // Teardown has started — refuse to (re)load. Admitting a fresh model/context
  // here would race disposeLlama() freeing the device. Throwing matches the
  // existing contract: best-effort passes catch → null, the summariser surfaces
  // → extractive fallback. Either way nothing touches a half-freed device.
  if (disposing) throw new Error("llama summariser is shutting down");
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
 * A {@link ChatComplete} backed by one preloaded chat session — the
 * bundled runtime's single point of variation; the per-pass logic lives
 * in `../pipeline/passes.ts`. `pick` selects which session runs the
 * completion (each was created in `loadOnce` with its pass's system
 * prompt baked in, so the port ignores `req.system`).
 *
 * Serialisation invariant (do not change lightly): the chat session has
 * ONE context sequence, so concurrent prompts would interleave. We chain
 * every call through the module `inflight` queue — `resetChatHistory()`
 * runs INSIDE the `.then` callback (never before queuing, or a later
 * caller could reset a call mid-flight), and `inflight` is reassigned to
 * THIS call's promise so the next caller awaits us. `loadOnce` throws
 * propagate: the summariser pass surfaces them (→ degrade), best-effort
 * passes catch them (→ null), exactly as before.
 */
function llamaChat(cfg: Required<LlamaConfig>, pick: (l: Loaded) => Session): ChatComplete {
  return async (req) => {
    const session = pick(await loadOnce(cfg));
    const run = inflight.then(async () => {
      // Re-check at the head of the queue: disposeLlama() may have armed
      // teardown while we were waiting our turn, and starting a prompt now
      // would race the device being freed.
      if (disposing) throw new Error("llama summariser is shutting down");
      session.resetChatHistory();
      const raw = await session.prompt(req.user, {
        temperature: req.temperature,
        maxTokens: req.maxTokens,
      });
      return stripThinking(raw ?? "");
    });
    inflight = run.catch(() => undefined);
    return run;
  };
}

/**
 * Summariser adapter — REQUIRED output, drop-in for `ollamaSummarize`.
 * Same prompt contract, same ≤240-char cap (both owned by
 * `makeSummarizer`). On first call, pulls the GGUF to disk and warms the
 * llama context; subsequent calls queue through the `inflight` serialiser.
 */
export function llamaSummarize(cfg: Required<LlamaConfig> = defaultLlamaConfig()): Summarizer {
  return makeSummarizer(llamaChat(cfg, (l) => l.summarizer));
}

/** Cognition adapter — best-effort `{ emotions, meta }` tags from the
 * post-redaction abstract, on the `COGNITION_SYSTEM_PROMPT` session.
 * Failure ⇒ null and the segment ships without the suffix. */
export function llamaCognize(cfg: Required<LlamaConfig> = defaultLlamaConfig()): Cognizer {
  return makeCognizer(llamaChat(cfg, (l) => l.cognizer));
}

/** Session-title adapter — best-effort, on the `TITLER_SYSTEM_PROMPT`
 * session. Failure ⇒ null and `buildSessionTitles` uses its
 * deterministic fallback. */
export function llamaEntitle(cfg: Required<LlamaConfig> = defaultLlamaConfig()): Entitler {
  return makeEntitler(llamaChat(cfg, (l) => l.entitler));
}

/** Per-script content-summary adapter — best-effort, on the
 * `SCRIPT_SUMMARY_SYSTEM_PROMPT` session. One factual sentence per script
 * FILE; failure ⇒ null and the call ships without a script abstract. The
 * file contents never leave the device on this local path. */
export function llamaScriptSummarize(
  cfg: Required<LlamaConfig> = defaultLlamaConfig(),
): ScriptSummarizer {
  return makeScriptSummarizer(llamaChat(cfg, (l) => l.scriptSummarizer));
}

/** Link-extraction adapter — best-effort, on the
 * `LINK_EXTRACT_SYSTEM_PROMPT` session. Surfaces PR/issue/commit/repo
 * refs from the redacted abstracts for clients whose logs carry no git
 * data; failure ⇒ null and detection falls back to the deterministic
 * git + content channels. */
export function llamaExtractLinks(
  cfg: Required<LlamaConfig> = defaultLlamaConfig(),
): LinkExtractor {
  return makeLinkExtractor(llamaChat(cfg, (l) => l.linkExtractor));
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
      if (disposing) throw new Error("llama summariser is shutting down");
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
