/**
 * On-device NER/PII redaction adapter (Transformers.js / ONNX).
 *
 * Builds a Redactor function the daemon pipeline runs AFTER the regex pass.
 * Running on-device means PII never leaves the machine — the whole point of
 * "privacy filter". It loads a token-classification (NER) model and redacts each
 * detected entity span (person, org, location, …) as `[REDACTED:<TYPE>]`. The
 * BIO span-merging below is model-AGNOSTIC, so any standard NER model works.
 *
 * Default model: `Xenova/bert-base-NER` (dtype q8). NOTE: the previous default
 * `openai/privacy-filter` is NOT loadable by Transformers.js — its `model_type`
 * ("openai_privacy_filter") is unsupported, so that adapter ALWAYS threw at load
 * and fell through to pass-through (the NER layer never actually ran).
 * bert-base-NER is a standard, supported architecture that loads on-device and
 * detects person/org/location entities; structured PII (emails, keys, paths) is
 * covered by the regex floor and the local-LLM backstop around this layer.
 *
 * Loading is lazy: the model downloads on first call and caches in IndexedDB
 * (browser) or ~/.cache/huggingface (Node). If @huggingface/transformers isn't
 * installed, the adapter logs once and returns a no-op pass-through redactor, so
 * the pipeline always falls through to the regex result + LLM backstop.
 *
 * Default device:
 *   • Browser: "webgpu" (Transformers.js auto-falls-back to "wasm")
 *   • Node:    "cpu"    (onnxruntime-node)
 *
 * IMPORTANT: this is an OPTIONAL adapter. The consuming package (apps/daemon)
 * declares `@huggingface/transformers` and stages it beside the bundle; we use a
 * runtime dynamic import so missing the dep doesn't break the build for
 * consumers that don't care.
 */

import type { RedactionResult } from "@modelstat/core/redact";
import {
  isMissingOptionalModuleError,
  OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS,
} from "../optional-module.js";

export type Redactor = (text: string) => Promise<RedactionResult>;

export interface PrivacyFilterAdapterOptions {
  /** Override the model id. Defaults to "Xenova/bert-base-NER" (a Transformers.js-
   * compatible token-classification model). Any standard BIO-tagged NER model works. */
  model?: string;
  /** Device hint passed through to Transformers.js. Default: webgpu
   * in the browser, cpu in Node. The library auto-falls-back to
   * `wasm` if WebGPU is unavailable. */
  device?: "webgpu" | "wasm" | "cpu";
  /** Quantisation of the on-device weights. q8 is the default — small + fast on
   * CPU and the variant bert-base-NER publishes (it has no q4 build). */
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

/** Escape a string for literal use inside a RegExp. */
function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Reconstruct an entity's surface text from its WordPiece tokens.
 * `##`-prefixed tokens are continuations of the previous word (joined
 * with no space); every other token starts a new word (space-joined).
 * e.g. ["Globe", "##x", "Corporation"] → "Globex Corporation". */
export function reconstructSurface(words: string[]): string {
  let s = "";
  for (const w of words) {
    if (w.startsWith("##")) s += w.slice(2);
    else s += s ? ` ${w}` : w;
  }
  return s.trim();
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
  const dtype = opts.dtype ?? "q8";
  const modelId = opts.model ?? "Xenova/bert-base-NER";

  // Loose typing — the pipeline() return type is a complex union and
  // typing it strictly drags in @huggingface/transformers as a hard
  // dependency. We only need the call signature here.
  type TokenClassifier = (text: string) => Promise<
    Array<{
      entity: string;
      score: number;
      word: string;
      /** Position of the token in the tokenised sequence. A gap between
       * two entity tokens means an `O` token was dropped between them. */
      index?: number;
      /** Character offsets into the input. Transformers.js OMITS these for
       * several models (incl. bert-base-NER) — the redactor falls back to
       * surface-string reconstruction when they're absent. */
      start?: number;
      end?: number;
    }>
  >;

  const importModule = opts.importModule ?? ((id: string) => import(/* @vite-ignore */ id));

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
            ...(opts.onProgress ? { progress_callback: opts.onProgress } : {}),
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

  return async function redactWithPrivacyFilter(text: string): Promise<RedactionResult> {
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

    // Group consecutive entity tokens into spans. Transformers.js doesn't
    // expose Python's aggregation_strategy, so it returns one entry per
    // subword (O tokens already dropped) each with its position `index`.
    // Same `type` + contiguous `index` + not a fresh `B-` tag = one entity
    // (covers multi-word names AND `##` subwords); an index gap marks a
    // dropped O token between two adjacent entities.
    type NerToken = Awaited<ReturnType<TokenClassifier>>[number];
    const spans: Array<{ type: string; tokens: NerToken[] }> = [];
    let prevIndex: number | null = null;
    for (const t of tokens) {
      const ent = t.entity ?? "";
      if (!ent || ent === "O" || ent === "0") {
        prevIndex = null;
        continue;
      }
      const type = ent.replace(/^[BILUE]-/, "").toUpperCase();
      const idx = typeof t.index === "number" ? t.index : null;
      const last = spans[spans.length - 1];
      const continues =
        last != null &&
        last.type === type &&
        !/^B-/i.test(ent) &&
        idx != null &&
        prevIndex != null &&
        idx === prevIndex + 1;
      if (continues) last.tokens.push(t);
      else spans.push({ type, tokens: [t] });
      prevIndex = idx;
    }

    // Redact each span. Prefer precise character offsets when the model
    // provides them; otherwise — the common case on Transformers.js, which
    // omits offsets for bert-base-NER — reconstruct the entity's surface
    // text from its WordPiece tokens and redact its WORD-BOUNDARY
    // occurrences. This trades precision for recall: an identical name
    // appearing elsewhere is also redacted (the model emits one entity per
    // occurrence anyway, and over-redacting a lossy summary abstract is an
    // acceptable cost). Word boundaries — NOT raw substring — are what keep
    // it from mangling a superstring (redacting the person "Mark" must not
    // touch "Marketing").
    let out = text;
    const extra: Record<string, number> = {};
    const bump = (type: string, n = 1): void => {
      const key = `pf_${type.toLowerCase()}`;
      extra[key] = (extra[key] ?? 0) + n;
    };

    const haveOffsets =
      spans.length > 0 &&
      spans.every((s) =>
        s.tokens.every((t) => t.start != null && t.end != null && t.end > t.start),
      );

    if (haveOffsets) {
      const ranges = spans
        .map((s) => ({
          type: s.type,
          start: Math.min(...s.tokens.map((t) => t.start as number)),
          end: Math.max(...s.tokens.map((t) => t.end as number)),
        }))
        // Apply right-to-left so earlier offsets stay valid as we splice.
        .sort((a, b) => b.start - a.start);
      for (const r of ranges) {
        bump(r.type);
        out = `${out.slice(0, r.start)}[REDACTED:${r.type}]${out.slice(r.end)}`;
      }
    } else {
      // Longest surface first so "Ada Lovelace" is redacted before "Ada".
      const surfaces = new Map<string, string>();
      for (const s of spans) {
        const surface = reconstructSurface(s.tokens.map((t) => t.word));
        if (surface && !surfaces.has(surface)) surfaces.set(surface, s.type);
      }
      for (const [surface, type] of [...surfaces].sort((a, b) => b[0].length - a[0].length)) {
        // Boundary = "not flanked by a letter or digit" (Unicode-aware), so
        // "Mark" matches the standalone name but not the "Mark" inside
        // "Marketing"/"Markdown".
        const re = new RegExp(
          `(?<![\\p{L}\\p{N}])${escapeRegExp(surface)}(?![\\p{L}\\p{N}])`,
          "gu",
        );
        let n = 0;
        out = out.replace(re, () => {
          n += 1;
          return `[REDACTED:${type}]`;
        });
        if (n > 0) bump(type, n);
      }
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
