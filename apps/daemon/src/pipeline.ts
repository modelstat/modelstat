/**
 * CLI-side pipeline binding — wires the summariser/embedder pair the
 * daemon runs and hands them to daemon-core.
 *
 * The summariser runtime follows the install-time MODE (state.summarizerMode;
 * see runtime-state.ts). Redaction — the regex floor plus the on-device NER/PII
 * pass — runs client-side in EVERY mode; only the summarisation LOCATION moves:
 *
 *   - `local`       — the bundled `node-llama-cpp` runtime. The daemon ships
 *     `node-llama-cpp` and stages this platform's prebuilt binary beside the
 *     bundle at install time (service.ts `installNativeRuntime`); a ~2.7 GB
 *     Qwen3.5-4B GGUF lazy-downloads to `~/.modelstat/models/`. This is the
 *     ONLY mode that touches the local model.
 *   - `self-hosted` — summarise via an org-run OpenAI-compatible endpoint (its
 *     URL + model chosen at install; see `remoteAdapters`). EXPLICIT egress:
 *     prompt content is NER/PII-scrubbed on-device before it leaves the box.
 *   - `cloud`       — no local summariser at all. The scan loop ships redacted
 *     turns to /v1/ingest/raw and modelstat's cloud summarises server-side
 *     (see scan.ts). `getAdapters()` is only reached here as the NER-down
 *     fallback, which degrades to local extractive abstracts (no egress).
 *
 * A degraded extractive fallback keeps ingest alive whenever the selected
 * runtime can't run (bundled binary won't load, self-hosted endpoint
 * misconfigured, NER redactor unavailable): the dependency-free heuristic
 * summariser ships real, if plainer, abstracts rather than blocking. Because
 * summarisation is core product output the degradation is LOUD (a one-time
 * startup warning), never a silent no-op.
 *
 * The adapters are built once and cached for the life of the process.
 */
import { existsSync } from "node:fs";
import { readFile as fsReadFile } from "node:fs/promises";
import type { RawEvent, Segment, SessionMetadata } from "@modelstat/core";
import { redact as redactFloor } from "@modelstat/core/redact";
import {
  applyTransformersCacheDir,
  createTransformersJsEmbedder,
  defaultLlamaConfig,
  llamaCognize,
  llamaEntitle,
  llamaExtractLinks,
  llamaRedact,
  llamaScriptSummarize,
  llamaSummarize,
  makeOpenAICompatConfig,
  type OpenAICompatConfig,
  openaiCognize,
  openaiEntitle,
  openaiExtractLinks,
  openaiScriptSummarize,
  openaiSummarize,
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
import {
  createPrivacyFilterRedactor,
  nerRedactorActive,
} from "@modelstat/daemon-core/redact/privacy-filter";
import {
  checkPullRequestOutcome,
  collectFilesChanged,
  type LocalToolContext,
  resolveGitContext,
} from "@modelstat/parsers";
import { state } from "./config.js";
import { enrichToolCallRedaction, enrichToolCallScripts } from "./enrich-scripts.js";
import { runtimeState } from "./runtime-state.js";

let adapters: PipelineAdapters | null = null;

// The local 384-dim BGE-small embedder + the Qwen-family tokenizer
// heuristic are identical across every adapter set (bundled, remote,
// degraded) — embeddings ALWAYS stay on-device regardless of summariser
// provider. The embedder factory returns a closure that does its real
// (async, cached) work on first call, so constructing it once here is
// free until the first segment.
const localEmbed = createTransformersJsEmbedder();
const tokenize = (text: string): number => Math.max(1, Math.ceil(text.length / 4));

// ── Always-works summariser ────────────────────────────────────────────────
// The bundled Qwen LLM is the quality path, but it must NEVER block ingest. If
// it can't run on this machine (native runtime missing, model not downloaded,
// no network, incompatible binary, or even CPU load failing after the Metal
// guard), we degrade to the dependency-free extractive fallback so the daemon
// keeps shipping real abstracts instead of refusing to start.

let degradedThisProcess = false;
// After a CATCHABLE llm failure, skip the LLM for this long before retrying, so
// a persistent failure doesn't re-attempt on every single segment. The window
// is provider-aware: the LOCAL path guards against re-downloading the 2.7 GB
// GGUF on every miss, so it waits 10 min; the REMOTE path's failures are
// transient HTTP blips that resolve in seconds (and chatComplete already
// retried with backoff), so a long blackout would needlessly degrade a whole
// scan — it waits 1 min.
const LLM_RETRY_COOLDOWN_LOCAL_MS = 10 * 60_000;
const LLM_RETRY_COOLDOWN_REMOTE_MS = 60_000;
let llmRetryAfter = 0;

/** True if this process has fallen back to the extractive summariser at least
 * once — read by daemon.ts for the degraded status line + self-heal. */
export function summariserDegradedThisProcess(): boolean {
  return degradedThisProcess;
}

function markDegraded(reason: string, notice?: string): void {
  if (!degradedThisProcess) {
    degradedThisProcess = true;
    // biome-ignore lint/suspicious/noConsole: loud, one-time degradation notice
    console.warn(
      notice ??
        `[modelstat] ⚠ summariser DEGRADED — LLM unavailable (${reason}); shipping ` +
          "extractive fallback abstracts so ingest continues. They re-summarise at model " +
          "quality automatically once the LLM is healthy again.",
    );
  }
  // Persist so the NEXT start can self-heal (re-scan to upgrade these abstracts).
  runtimeState.setSummariserDegraded(true);
}

/**
 * The daemon's summariser: the provided LLM (bundled Qwen or remote) when it
 * works, the dependency-free extractive fallback when it can't. Per-call fallback on a
 * catchable LLM failure, debounced so a persistent failure doesn't hammer the
 * LLM (or its model download). An UNCATCHABLE native abort (e.g. the Metal
 * GGML_ASSERT) is handled one layer down by the CPU-fallback guard in
 * daemon-core/node/llama.ts. Never throws, never empty — ingest always proceeds.
 */
function resilientSummarize(llm: Summarizer, cooldownMs: number): Summarizer {
  const heuristic = heuristicSummarize();
  return async (input) => {
    if (Date.now() >= llmRetryAfter) {
      try {
        const out = await llm(input);
        if (out && out.trim().length > 0) return out;
        // Empty LLM output — treat as a transient miss; use the fallback for
        // this one call without entering cooldown (the model is loaded/fine).
      } catch (err) {
        llmRetryAfter = Date.now() + cooldownMs;
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
    embed: localEmbed,
    summarize: resilientSummarize(llamaSummarize(llamaCfg), LLM_RETRY_COOLDOWN_LOCAL_MS),
    tokenize,
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

type ResolvedProvider =
  | { readonly kind: "local" }
  | { readonly kind: "openai"; readonly cfg: OpenAICompatConfig }
  | { readonly kind: "cloud" }
  | { readonly kind: "misconfigured"; readonly reason: string };

let resolvedProvider: ResolvedProvider | null = null;

/** Resolve, ONCE, which summariser runtime the install-time mode selects —
 * never throwing. Self-hosted with an incomplete endpoint config resolves to
 * `misconfigured` (carrying the operator-facing reason) so the daemon degrades
 * to the extractive fallback with a loud notice instead of crashing mid-scan.
 * The result is cached: the mode is immutable for the process lifetime (a
 * `modelstat mode` change bounces the service), and every call site shares one
 * snapshot so e.g. the script summariser can't drift to a different endpoint. */
function resolveProvider(): ResolvedProvider {
  if (resolvedProvider) return resolvedProvider;
  const mode = state.summarizerMode;
  if (mode === "local") {
    resolvedProvider = { kind: "local" };
  } else if (mode === "cloud") {
    resolvedProvider = { kind: "cloud" };
  } else {
    // self-hosted — summarise via the org's OpenAI-compatible endpoint
    // (URL + model chosen at install, or overridden by MODELSTAT_LLM_* env).
    try {
      const { url, model } = state.selfHosted;
      resolvedProvider = {
        kind: "openai",
        cfg: makeOpenAICompatConfig(url, model, "self-hosted summariser URL"),
      };
    } catch (err) {
      resolvedProvider = { kind: "misconfigured", reason: (err as Error).message };
    }
  }
  return resolvedProvider;
}

/** Test-only: drop the cached provider so the next resolveProvider() re-reads
 * the mode. The daemon never needs this (a mode change bounces the process). */
export function _resetProviderForTests(): void {
  resolvedProvider = null;
  adapters = null;
  cloudRedactor = null;
}

/** Build the remote-egress scrubber: the deterministic regex floor
 * (always) followed by the on-device NER/PII pass (best-effort), applied
 * to every raw prompt body — sampled conversation excerpts AND script
 * file contents — BEFORE it is POSTed to the remote endpoint. This is the
 * half of redaction the local path gets for free: there raw text feeds an
 * on-device model so only the OUTPUT abstract needs scrubbing; here the
 * INPUT leaves the box, so it must be scrubbed first too. The same
 * `nerRedact` instance backs the output `redact` adapter — one model,
 * both directions. */
function makeRemotePreSend(nerRedact: Redactor): (text: string) => Promise<string> {
  return async (text: string): Promise<string> => {
    const floored = redactFloor(text).text;
    try {
      return (await nerRedact(floored)).text;
    } catch {
      // NER is best-effort; the regex floor already ran, so ship that.
      return floored;
    }
  };
}

/** Loud, one-time notice that summarisation now leaves the machine: the
 * summariser prompt (sampled, regex-floor-redacted excerpts) and script
 * bodies are sent to the remote endpoint, while embeddings + the
 * Privacy-Filter PII pass stay on-device. Surfaced at startup so it is
 * never silent — this product's promise is that raw content stays local. */
function warnRemoteEgress(cfg: OpenAICompatConfig): void {
  // biome-ignore lint/suspicious/noConsole: one-time egress notice
  console.warn(
    `[modelstat] ⚠ self-hosted summariser ENABLED — session excerpts + script bodies are sent to ${cfg.baseUrl} (model ${cfg.model}). ` +
      "Embeddings + on-device PII redaction stay local. Run `modelstat mode local` for the bundled on-device model.",
  );
}

/** Remote OpenAI-compatible adapter set. Chat passes go to the endpoint;
 * the embedder stays local (preserving the 384-dim wire vector) and the
 * redactor is the local Privacy Filter ONLY — no remote redaction
 * backstop, since asking a third party to scrub a likely-secret string
 * would exfiltrate the very secret. Takes the already-built (and
 * NER-verified) Privacy Filter so the egress scrubber and the output
 * redactor share one proven-live instance. */
function remoteAdapters(cfg: OpenAICompatConfig, privacyFilter: Redactor): PipelineAdapters {
  // One Privacy-Filter instance backs BOTH the pre-send egress scrubber
  // (excerpts + script bodies, on the way out) and the output `redact`
  // adapter (the returned abstract). cognize/entitle/extractLinks run over
  // already-redacted abstracts, so they need no pre-send pass.
  const preSend = makeRemotePreSend(privacyFilter);
  return {
    embed: localEmbed,
    summarize: resilientSummarize(openaiSummarize(cfg, preSend), LLM_RETRY_COOLDOWN_REMOTE_MS),
    tokenize,
    cognize: openaiCognize(cfg),
    entitle: openaiEntitle(cfg),
    extractLinks: openaiExtractLinks(cfg),
    redact: privacyFilter,
  };
}

/** Extractive-only adapter set: NO LLM at all (heuristic summariser +
 * local embedder + local PII redactor). Used when the remote provider is
 * misconfigured — keep ingest alive on the dependency-free fallback rather
 * than crash, and WITHOUT silently spinning up the 2.7 GB local model the
 * operator explicitly opted out of by choosing the remote provider. */
async function degradedAdapters(): Promise<PipelineAdapters> {
  return {
    embed: localEmbed,
    summarize: heuristicSummarize(),
    tokenize,
    redact: await createPrivacyFilterRedactor(),
  };
}

async function getAdapters(): Promise<PipelineAdapters> {
  if (adapters) return adapters;
  // Point Transformers.js (the NER redactor + the BGE embedder) at the shared
  // on-disk cache before the first model load, so every mode reads the model
  // `connect` warmed and upgrades don't re-download it.
  await applyTransformersCacheDir();
  const provider = resolveProvider();
  // Misconfigured remote provider (bad MODELSTAT_LLM_PROVIDER, or missing
  // base-url/model): never crash a scan over a config typo. Surface it loudly
  // with the fix, then ship extractive abstracts until the operator corrects
  // the env and restarts.
  if (provider.kind === "misconfigured") {
    markDegraded(
      provider.reason,
      `[modelstat] ⚠ self-hosted summariser MISCONFIGURED (${provider.reason}) — shipping ` +
        "extractive fallback abstracts so ingest continues. Fix the endpoint with " +
        "`modelstat mode self-hosted --url <URL> --model <ID>` and restart.",
    );
    adapters = await degradedAdapters();
    return adapters;
  }
  // Cloud mode: modelstat's cloud summarises server-side from the redacted
  // turns the scan loop ships to /v1/ingest/raw, so there is NO local summariser
  // to build. We only reach here as scan.ts's NER-down fallback — degrade to the
  // extractive set (local, no egress, no model) so ingest still ships abstracts.
  if (provider.kind === "cloud") {
    adapters = await degradedAdapters();
    return adapters;
  }
  // Self-hosted: summarise via an org-run OpenAI-compatible endpoint (for orgs
  // that want the model off modelstat's cloud but off the user's machine too).
  // Explicit egress — warn loudly, then build the remote set (embeddings + PII
  // redaction stay local).
  if (provider.kind === "openai") {
    // FAIL-CLOSED on the egress guard. The remote path's privacy promise is
    // that raw excerpts/script bodies are NER/PII-scrubbed on-device before
    // they leave the box. If that NER redactor is a silent pass-through
    // (its optional dep is missing), we must NOT ship raw content to a third
    // party with only the regex floor — so we refuse the remote path and
    // degrade to the extractive fallback (local, no egress) instead.
    const privacyFilter = await createPrivacyFilterRedactor();
    if (!(await nerRedactorActive(privacyFilter))) {
      markDegraded(
        "remote NER redactor unavailable",
        "[modelstat] ⚠ remote summariser DISABLED — the on-device NER/PII redactor " +
          "(@huggingface/transformers) isn't available, so session excerpts + script bodies " +
          "can't be scrubbed before leaving the machine. Shipping extractive fallback abstracts " +
          "(no egress) instead. Install @huggingface/transformers to enable the remote model.",
      );
      adapters = await degradedAdapters();
      return adapters;
    }
    warnRemoteEgress(provider.cfg);
    adapters = remoteAdapters(provider.cfg, privacyFilter);
    return adapters;
  }
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

// ── Cloud mode: redact turns on-device, summarise server-side ────────────────
// Cloud mode ships cleaned turns (not a local abstract) to /v1/ingest/raw, so
// the on-device NER/PII pass has to run over the EXCERPTS themselves — the
// parser already floor-redacted them, but the NER pass is what the normal path
// applies only to the derived abstract. The sentinel proof (nerRedactorActive)
// makes the fail-closed decision below honest; the healthy verdict is cached,
// an unhealthy one is re-probed each scan so cloud self-heals (see below).
let cloudRedactor: { redact: Redactor; nerActive: boolean } | null = null;

async function cloudRawRedactor(): Promise<{ redact: Redactor; nerActive: boolean }> {
  // Cache ONLY the healthy verdict. On a fresh install the NER model may still be
  // downloading on the first scan; caching an unhealthy verdict would wedge cloud
  // mode on local extractive abstracts until a daemon restart. So while degraded
  // we re-probe every scan — cheap, because the REUSED redactor's cooldown makes
  // a not-yet-ready load return instantly — and cloud SELF-HEALS the moment the
  // model lands (the 5-min backstop scan picks it up). Reuse the redactor
  // instance so its load cooldown/cache persist across probes.
  if (cloudRedactor?.nerActive) return cloudRedactor;
  await applyTransformersCacheDir();
  const redact = cloudRedactor?.redact ?? (await createPrivacyFilterRedactor());
  cloudRedactor = { redact, nerActive: await nerRedactorActive(redact) };
  return cloudRedactor;
}

/**
 * Prepare a Cloud-mode raw batch: run the FULL redaction pipeline over every
 * event excerpt and tool-call command before they leave the machine. The regex
 * floor already ran in the parser; this adds the on-device NER/PII pass, so the
 * turns modelstat's cloud summarises are secrets- AND names/orgs-scrubbed.
 *
 * FAIL-CLOSED: returns `null` when the on-device NER redactor is unavailable
 * (its `@huggingface/transformers` peer dep is missing / a silent pass-through).
 * The caller then degrades to LOCAL extractive abstracts — NO raw egress —
 * rather than shipping floor-only-redacted turns off the box. Mutates `drafts`
 * in place (their `command_redacted` gets the NER pass) and returns the redacted
 * events; the tool-call floor already ran in the parser too.
 *
 * NOTE: the daemon's parsers cap `content_excerpt` at 320 chars, so Cloud ships
 * the same per-turn excerpts the local model would summarise — not literally
 * untruncated turns like the raw SDK. Widening that is a parser change (a
 * follow-up), not part of this client wiring.
 */
export async function prepareCloudRawEvents(
  events: RawEvent[],
  drafts: readonly ToolCallDraft[],
): Promise<RawEvent[] | null> {
  const { redact, nerActive } = await cloudRawRedactor();
  if (!nerActive) return null; // fail-closed — caller keeps data local
  const redacted = await Promise.all(
    events.map(async (e) => {
      if (!e.content_excerpt) return e;
      const floored = redactFloor(e.content_excerpt).text;
      const scrubbed = (await redact(floored)).text;
      return scrubbed === e.content_excerpt ? e : { ...e, content_excerpt: scrubbed };
    }),
  );
  // Tool-call commands leave the box too — give command_redacted the same NER
  // pass (its floor already ran in the parser). No LLM script summaries here.
  if (drafts.length) await enrichToolCallRedaction(drafts, redact);
  return redacted;
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
    collectFilesChanged,
  });
}

/** Max bytes read from any one script before summarising. Scripts are small;
 * this bounds memory + model input (the prompt builder slices further). */
const MAX_SCRIPT_READ_BYTES = 64 * 1024;

let scriptSummarizer: ScriptSummarizer | null = null;

/** The per-script content summariser for the active provider. Remote when
 * MODELSTAT_LLM_PROVIDER=openai — script bodies leave the box, so they go
 * through the same pre-send NER/PII scrubber as the summariser excerpts
 * (`modelRedact` is the local Privacy Filter from the cached adapter set).
 * Bundled on-device model otherwise. Misconfigured → a no-op summariser so
 * we neither call out nor spin up the opted-out local model. */
function buildScriptSummarizer(modelRedact: Redactor | undefined): ScriptSummarizer {
  const provider = resolveProvider();
  // Cloud summarises server-side and misconfigured has no runtime — neither
  // should spin up the opted-out local model, so both no-op.
  if (provider.kind === "misconfigured" || provider.kind === "cloud") return async () => null;
  if (provider.kind === "openai") {
    const preSend = modelRedact ? makeRemotePreSend(modelRedact) : undefined;
    return openaiScriptSummarize(provider.cfg, preSend);
  }
  return llamaScriptSummarize(defaultLlamaConfig());
}

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
  if (!scriptSummarizer) scriptSummarizer = buildScriptSummarizer(built.redact);
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
  // Cloud mode has no local summariser to exercise — modelstat's cloud
  // summarises the redacted turns server-side. Report it plainly; a missing
  // local model can't "degrade" ingest here. (If the on-device NER redactor is
  // unavailable at scan time, scan.ts falls back to local extractive abstracts
  // and marks degraded there — the honest place to detect it.)
  if (resolveProvider().kind === "cloud") {
    return { label: "cloud — modelstat summarises server-side (no local model)", degraded: false };
  }
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
  const provider = resolveProvider();
  const engine = degraded
    ? "extractive fallback (LLM unavailable)"
    : provider.kind === "openai"
      ? `remote ${provider.cfg.model}`
      : "Qwen3.5-4B";
  return { label: `${engine} — "${sample}"`, degraded };
}
