/**
 * BGE-small (bge-small-en-v1.5) embeddings via @huggingface/transformers
 * (ONNX + WebGPU, falls back to WASM). ~33MB model weights, cached in OPFS
 * / browser cache by transformers.js; first use downloads, subsequent
 * loads are instant.
 *
 * The model id comes from the shared pipeline contract (BROWSER_EMBED_MODEL)
 * — the browser and the CLI MUST embed into the same 384-dim vector space, or
 * cross-source cosine similarity is meaningless (vectors from different models
 * are not comparable).
 *
 * Used for:
 *   - Zero-shot categorization (cosine-sim vs taxonomy prototypes)
 *   - Near-duplicate detection across conversations
 *
 * IMPORTANT: this runs in the offscreen document, not the SW — the
 * SW's lifecycle is hostile to long-lived model instances.
 */

import { pipeline, type FeatureExtractionPipeline } from "@huggingface/transformers";
import { BROWSER_EMBED_MODEL } from "@modelstat/daemon-core/pipeline";
import { createLogger } from "@/common/logger.js";

const log = createLogger("embed");

let extractorPromise: Promise<FeatureExtractionPipeline> | null = null;

export async function getEmbedder(): Promise<FeatureExtractionPipeline> {
  if (extractorPromise) return extractorPromise;
  extractorPromise = (async () => {
    log.info(`loading ${BROWSER_EMBED_MODEL} (first run may download ~33MB)`);
    const pipe = await pipeline("feature-extraction", BROWSER_EMBED_MODEL, {
      // Prefer WebGPU when available; transformers.js auto-falls-back to WASM.
      device: "webgpu",
    }).catch(async () => {
      log.info("webgpu unavailable — using wasm");
      return pipeline("feature-extraction", BROWSER_EMBED_MODEL, { device: "wasm" });
    });
    return pipe as FeatureExtractionPipeline;
  })();
  return extractorPromise;
}

export async function embed(text: string): Promise<number[]> {
  const pipe = await getEmbedder();
  // Normalise + mean-pool for sentence embeddings.
  const out = await pipe(text, { pooling: "mean", normalize: true });
  // transformers.js returns a Tensor; .tolist() → number[][]; pick [0]
  const arr = (out as unknown as { tolist(): number[][] }).tolist();
  return arr[0] ?? [];
}

export function cosineSim(a: number[], b: number[]): number {
  const n = Math.min(a.length, b.length);
  let dot = 0;
  let na = 0;
  let nb = 0;
  for (let i = 0; i < n; i++) {
    const ai = a[i] ?? 0;
    const bi = b[i] ?? 0;
    dot += ai * bi;
    na += ai * ai;
    nb += bi * bi;
  }
  if (na === 0 || nb === 0) return 0;
  return dot / (Math.sqrt(na) * Math.sqrt(nb));
}

/** Classify text against a set of labelled prototype sentences.
 * Returns the label with highest cosine similarity + its score. */
export async function classifyZeroShot(
  text: string,
  prototypes: Record<string, string>,
): Promise<{ label: string; score: number }> {
  const qv = await embed(text);
  let best = { label: "", score: -1 };
  for (const [label, proto] of Object.entries(prototypes)) {
    const pv = await embed(proto);
    const score = cosineSim(qv, pv);
    if (score > best.score) best = { label, score };
  }
  return best;
}
