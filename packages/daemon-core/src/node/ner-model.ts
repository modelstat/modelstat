/**
 * Pre-warm the on-device NER/PII redaction model.
 *
 * Redaction (regex floor + on-device NER/PII) runs in EVERY summariser mode, and
 * the cloud + self-hosted paths are FAIL-CLOSED on it: if the NER model isn't
 * live they refuse egress and degrade to local extractive abstracts. The model
 * (`Xenova/bert-base-NER`, ~250 MB) otherwise downloads LAZILY on the daemon's
 * first scan — so a fresh install races that download and starts degraded until
 * it lands (and, without a self-healing re-probe, can stay degraded until a
 * restart). Pre-warming at `connect` closes that window: the model is on disk,
 * in the SHARED cache dir the daemon reads ({@link applyTransformersCacheDir}),
 * BEFORE the service starts.
 */
import {
  createPrivacyFilterRedactor,
  nerRedactorActive,
  type PrivacyFilterAdapterOptions,
} from "../redact/privacy-filter.js";
import { applyTransformersCacheDir } from "./transformers-cache.js";

/**
 * Load the NER/PII redactor (triggering the model download into the shared cache
 * dir) and PROVE it actually scrubs a sentinel PERSON. Returns `true` only when
 * the model is LIVE — a pass-through (missing dep) or a not-yet-finished download
 * returns `false`. Never throws, so a warm can be best-effort: a `false` just
 * means the daemon finishes the download on its first scan (and self-heals).
 *
 * Downloading the model can transiently report "not ready" if the probe reads a
 * file mid-download; that's harmless — the download still completes and the next
 * probe (connect re-run, or the daemon's re-probe) succeeds.
 */
export async function ensureNerModel(opts: PrivacyFilterAdapterOptions = {}): Promise<boolean> {
  // Point Transformers.js at the shared cache BEFORE the first load, so the warm
  // lands where the daemon reads. Skipped when a test injects a fake importer
  // (no real module to configure, no filesystem to touch).
  if (!opts.importModule) await applyTransformersCacheDir();
  const redact = await createPrivacyFilterRedactor(opts);
  return nerRedactorActive(redact);
}
