/**
 * Node-side Embedder backed by @huggingface/transformers (ONNX +
 * onnxruntime-node) running BAAI/bge-small-en-v1.5.
 *
 * Why this exists: the wire embedding is 384-dim (BGE-small). The CLI
 * and browser companions must produce vectors from the SAME model so a
 * segment embedded by either is directly comparable (cosine similarity
 * meaningful across runtimes). We run the model via transformers.js —
 * the optional peer dep is already required by the on-device privacy
 * filter, so this adds no new dependency footprint for companions
 * that have both adapters wired.
 *
 * Browser counterpart lives in packages/companion-core/src/browser/
 * (transformers.js auto-picks WebGPU there); same model, same vector
 * space, same shape.
 *
 * Loading is lazy — first call downloads ~50 MB of ONNX weights to
 * ~/.cache/huggingface/transformers/, subsequent calls reuse the
 * loaded pipeline. If the optional peer dep is missing OR loading
 * fails, the adapter returns an empty vector — the pipeline's
 * segment-boundary detection falls back to a time-gap heuristic and
 * the segment still ships (the abstract embedding is simply omitted).
 * No throws.
 */
import {
  isMissingOptionalModuleError,
  OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS,
} from "../optional-module.js";
import type { Embedder } from "../pipeline/index.js";

interface FeatureExtractor {
  (
    text: string | string[],
    opts?: { pooling?: "mean" | "cls"; normalize?: boolean },
  ): Promise<{ data: Float32Array; dims: number[] }>;
}

let cached: FeatureExtractor | null = null;
let loadPromise: Promise<FeatureExtractor | null> | null = null;
let loadFailedPermanently = false;
let loadAttempts = 0;
let warnedUnavailable = false;
let warnedInferenceError = false;

const DEFAULT_MODEL = "Xenova/bge-small-en-v1.5";

/** Indirection over dynamic import so tests can simulate a missing /
 * flaky module without uninstalling anything. Production always uses
 * the real `import()`. */
type ModuleImporter = (id: string) => Promise<unknown>;
let importModule: ModuleImporter = (id) => import(/* @vite-ignore */ id);

/** Test hook: replace the dynamic importer and reset all module-level
 * loader state. Not for production use. */
export function __setImporterForTests(importer: ModuleImporter | null): void {
  importModule = importer ?? ((id) => import(/* @vite-ignore */ id));
  cached = null;
  loadPromise = null;
  loadFailedPermanently = false;
  loadAttempts = 0;
  warnedUnavailable = false;
  warnedInferenceError = false;
}

async function loadPipeline(model: string): Promise<FeatureExtractor | null> {
  if (cached) return cached;
  // Latched-unavailable answers immediately: embed() sits on the
  // per-turn hot path of a scan, and a full-corpus reprocess calls it
  // millions of times. Re-running a FAILING dynamic import() from here
  // is what OOM-crashed the daemon on 2026-06-11 (~5M failed imports,
  // 8 GB heap, ~1 GB of identical warn lines) — see optional-module.ts.
  if (loadFailedPermanently) return null;
  if (!loadPromise) {
    loadPromise = (async () => {
      try {
        // Indirect import via a string variable so TypeScript doesn't
        // try to resolve @huggingface/transformers at typecheck time
        // — it's an optional peer dep that consumers install
        // themselves (e.g. apps/agent-dev pulls it in because the
        // privacy-filter adapter also needs it). Matches the pattern
        // used in src/redact/privacy-filter.ts.
        const tjs = (await importModule("@huggingface/transformers")) as unknown as {
          pipeline: (
            task: string,
            model: string,
            opts?: Record<string, unknown>,
          ) => Promise<FeatureExtractor>;
        };
        const p = await tjs.pipeline("feature-extraction", model, {
          device: "cpu",
          dtype: "fp32",
        });
        cached = p;
        return p;
      } catch (err) {
        const msg = (err as Error).message;
        loadAttempts += 1;
        if (
          isMissingOptionalModuleError(err) ||
          /unsupported|architecture|not supported|onnx/i.test(msg) ||
          loadAttempts >= OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS
        ) {
          loadFailedPermanently = true;
        }
        if (!warnedUnavailable) {
          warnedUnavailable = true;
          // biome-ignore lint/suspicious/noConsole: one-line, user-visible warning
          console.warn(
            `[modelstat] transformers.js embedder unavailable (segments will be re-embedded server-side; further attempts ${
              loadFailedPermanently ? "disabled" : `limited to ${OPTIONAL_MODULE_MAX_LOAD_ATTEMPTS}`
            }, warning once): ${msg}`,
          );
        }
        loadPromise = null;
        return null;
      }
    })();
  }
  return loadPromise;
}

/** Factory — returns an Embedder that fits the pipeline contract.
 * Pass an explicit model id to override the default; useful for
 * eval scripts comparing variants. */
export function createTransformersJsEmbedder(model: string = DEFAULT_MODEL): Embedder {
  return async (text: string): Promise<number[]> => {
    if (!text || text.trim().length === 0) return [];
    const pipe = await loadPipeline(model);
    if (!pipe) return [];
    try {
      // bge-small uses mean-pooling + L2 normalisation, so the vectors
      // land on the unit sphere — directly comparable across runtimes.
      const out = await pipe(text, { pooling: "mean", normalize: true });
      return Array.from(out.data);
    } catch (err) {
      // Warn once, not per call — embed() runs once per turn, so a
      // deterministic inference failure would otherwise flood the log
      // for the entire scan (the err.log-grows-to-1GB failure mode).
      if (!warnedInferenceError) {
        warnedInferenceError = true;
        // biome-ignore lint/suspicious/noConsole: one-line warning
        console.warn(
          `[modelstat] embed error (returning empty vectors, server will re-embed; warning once): ${(err as Error).message}`,
        );
      }
      return [];
    }
  };
}
