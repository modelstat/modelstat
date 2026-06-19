/**
 * Client-side OpenAI Privacy Filter adapter.
 *
 * Builds a Redactor function that the daemon pipeline runs AFTER
 * the regex pass. Running on-device means PII never leaves the
 * machine, which is the whole point of "privacy filter".
 *
 * Loading is lazy: the model file (~1 GB at q4) downloads on first
 * call and caches in IndexedDB (browser) or ~/.cache/huggingface
 * (Node). If the consumer hasn't installed @huggingface/transformers
 * — which we don't depend on directly to keep daemon-core's tree
 * lean — the adapter logs once and returns a no-op redactor that
 * passes text through unchanged. The pipeline always falls through to
 * the regex result in that case.
 *
 * Default device:
 *   • Browser: "webgpu" (Transformers.js auto-falls-back to "wasm")
 *   • Node:    "cpu"    (onnxruntime-node)
 *
 * Daemons that don't want the model (small CPU device, slow disk,
 * privacy posture already covered by regex) just don't call this
 * builder — the pipeline runs without a redact adapter and the regex
 * result is the only output, identical to the pre-filter behaviour.
 *
 * IMPORTANT: this is an OPTIONAL adapter. To use it, the consuming
 * package (the daemon CLI / browser extension repo) must add
 * `@huggingface/transformers` as a dependency itself. We use a
 * runtime dynamic import so missing the dep doesn't break the
 * pipeline build for consumers that don't care.
 */

import type { RedactionResult } from "@modelstat/core/redact";
import {
  isMissingOptionalModuleError,
  OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS,
} from "../optional-module.js";

export type Redactor = (text: string) => Promise<RedactionResult>;

export interface PrivacyFilterAdapterOptions {
  /** Override the model id. Defaults to "openai/privacy-filter". */
  model?: string;
  /** Device hint passed through to Transformers.js. Default: webgpu
   * in the browser, cpu in Node. The library auto-falls-back to
   * `wasm` if WebGPU is unavailable. */
  device?: "webgpu" | "wasm" | "cpu";
  /** Quantisation of the on-device weights. q4 is the recommended
   * default — ~1 GB on disk, recall stays >97% per OpenAI's card. */
  dtype?: "fp32" | "fp16" | "q8" | "q4";
  /** Optional progress callback — Transformers.js fires this during
   * the model download so the UI can show a "loading 32%" indicator
   * the first time the user opens an extension popup or runs the
   * CLI. Receives Transformers.js's progress events as-is. */
  onProgress?: (e: { status?: string; progress?: number; file?: string }) => void;
  /** Test hook: replace the dynamic `import()` used to load
   * @huggingface/transformers so tests can simulate a missing or flaky
   * module. Production callers leave this unset. */
  importModule?: (id: string) => Promise<unknown>;
}

interface MergedSpan {
  type: string;
  start: number;
  end: number;
}

/**
 * Build a Redactor backed by OpenAI Privacy Filter.
 *
 * The returned function is safe to call from the start — model
 * loading is deferred until the first invocation, then cached. If
 * loading ever fails (no Transformers.js, no network, etc.) we fall
 * back to a pass-through that returns the input verbatim with empty
 * counts; the pipeline's regex result is what the segment ends up
 * carrying.
 */
export async function createPrivacyFilterRedactor(
  opts: PrivacyFilterAdapterOptions = {},
): Promise<Redactor> {
  const isBrowser =
    typeof globalThis !== "undefined" &&
    typeof (globalThis as { window?: unknown }).window !== "undefined";
  const device = opts.device ?? (isBrowser ? "webgpu" : "cpu");
  const dtype = opts.dtype ?? "q4";
  const modelId = opts.model ?? "openai/privacy-filter";

  // Loose typing — the pipeline() return type is a complex union and
  // typing it strictly drags in @huggingface/transformers as a hard
  // dependency. We only need the call signature here.
  type TokenClassifier = (
    text: string,
  ) => Promise<
    Array<{
      entity: string;
      score: number;
      word: string;
      start?: number;
      end?: number;
    }>
  >;

  const importModule =
    opts.importModule ?? ((id: string) => import(/* @vite-ignore */ id));

  let cached: TokenClassifier | null = null;
  let loadPromise: Promise<TokenClassifier | null> | null = null;
  let loadFailedPermanently = false;
  let loadAttempts = 0;
  let warnedUnavailable = false;
  let warnedInferenceError = false;

  async function loadPipeline(): Promise<TokenClassifier | null> {
    if (cached) return cached;
    // Latched-unavailable answers immediately. The redactor runs once
    // per segment during a scan; re-running a FAILING dynamic import()
    // from here is the same heap leak + log flood that OOM-crashed the
    // daemon during the 2026-06-11 full reprocess — see the embedder in
    // ../node/transformersjs-embed.ts and ../optional-module.ts.
    if (loadFailedPermanently) return null;
    if (!loadPromise) {
      loadPromise = (async () => {
        try {
          // Dynamic import via an injectable indirection so TypeScript
          // doesn't statically resolve the module. @huggingface/
          // transformers is an OPTIONAL peer dep; consumers who don't
          // need on-device redaction shouldn't have to install it.
          const tjs = (await importModule("@huggingface/transformers")) as unknown as {
            pipeline: (
              task: string,
              model: string,
              opts?: Record<string, unknown>,
            ) => Promise<TokenClassifier>;
          };
          const p = await tjs.pipeline("token-classification", modelId, {
            device,
            dtype,
            ...(opts.onProgress
              ? { progress_callback: opts.onProgress }
              : {}),
          });
          cached = p;
          return p;
        } catch (err) {
          loadPromise = null;
          loadAttempts += 1;
          if (
            isMissingOptionalModuleError(err) ||
            loadAttempts >= OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS
          ) {
            loadFailedPermanently = true;
          }
          if (!warnedUnavailable) {
            warnedUnavailable = true;
            // eslint-disable-next-line no-console
            console.warn(
              "[privacy-filter] adapter unavailable — install @huggingface/transformers in the consuming package to enable model-based redaction. Falling back to pass-through (warning once).",
              (err as Error).message,
            );
          }
          return null;
        }
      })();
    }
    return loadPromise;
  }

  return async function redactWithPrivacyFilter(
    text: string,
  ): Promise<RedactionResult> {
    const empty: RedactionResult = {
      text,
      counts: {
        secrets_found: 0,
        emails_redacted: 0,
        paths_redacted_absolute: 0,
      },
    };
    if (!text) return empty;
    const classify = await loadPipeline();
    if (!classify) return empty;

    let tokens: Awaited<ReturnType<TokenClassifier>>;
    try {
      tokens = await classify(text);
    } catch (err) {
      // Warn once, not per segment — a deterministic inference failure
      // would otherwise repeat for every segment of a long scan.
      if (!warnedInferenceError) {
        warnedInferenceError = true;
        // eslint-disable-next-line no-console
        console.warn(
          "[privacy-filter] inference failed, returning input unchanged (warning once):",
          (err as Error).message,
        );
      }
      return empty;
    }

    // Merge BIO-tagged tokens. Transformers.js 3.x doesn't expose
    // aggregation_strategy on the JS pipeline (Python-only), so the
    // model returns one entry per subword. We do span-merging here so
    // the redaction is deterministic for a given input.
    const spans: MergedSpan[] = [];
    for (const t of tokens) {
      const ent = t.entity ?? "";
      if (!ent || ent === "O" || ent === "0") continue;
      if (t.start == null || t.end == null || t.end <= t.start) continue;
      const type = ent.replace(/^[BILUE]-/, "").toUpperCase();
      const last = spans[spans.length - 1];
      if (last && last.type === type && t.start - last.end <= 2) {
        last.end = t.end;
      } else {
        spans.push({ type, start: t.start, end: t.end });
      }
    }

    spans.sort((a, b) => b.start - a.start);
    let out = text;
    const extra: Record<string, number> = {};
    for (const s of spans) {
      extra[`pf_${s.type.toLowerCase()}`] =
        (extra[`pf_${s.type.toLowerCase()}`] ?? 0) + 1;
      out = out.slice(0, s.start) + `[REDACTED:${s.type}]` + out.slice(s.end);
    }

    return {
      text: out,
      counts: {
        secrets_found: 0,
        emails_redacted: 0,
        paths_redacted_absolute: 0,
        ...extra,
      },
    };
  };
}
