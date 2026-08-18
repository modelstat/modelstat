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
 * IMPORTANT: this is an OPTIONAL adapter. The consuming package declares
 * `@huggingface/transformers` and stages it beside the bundle; we use a
 * runtime dynamic import so missing the dep doesn't break the build for
 * consumers that don't care.
 */

import type { RedactionResult } from "@modelstat/core/redact";
import { isMissingOptionalModuleError } from "../optional-module.js";

export type Redactor = (text: string) => Promise<RedactionResult>;

/** After a TRANSIENT model-load failure (as opposed to a genuinely-missing
 * package), wait this long before the next real load attempt. Bounds retries to
 * ~once/minute so a persistent failure can never storm the loader, while still
 * recovering well within a scan cycle once the model finishes downloading. */
const DEFAULT_RETRY_COOLDOWN_MS = 60_000;

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
  /** Cooldown (ms) before retrying a TRANSIENT load failure. Defaults to
   * {@link DEFAULT_RETRY_COOLDOWN_MS}. Tests shrink it to exercise self-heal. */
  retryCooldownMs?: number;
  /** Injectable clock backing the retry cooldown. Defaults to `Date.now`.
   * Tests pass a controllable clock so a cooldown can be crossed deterministically. */
  now?: () => number;
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
  type TokenClassifier = (
    text: string,
  ) => Promise<Array<{ entity: string; word: string; start?: number; end?: number }>>;

  const importModule = opts.importModule ?? ((id: string) => import(/* @vite-ignore */ id));
  const retryCooldownMs = opts.retryCooldownMs ?? DEFAULT_RETRY_COOLDOWN_MS;
  const now = opts.now ?? Date.now;

  let cached: TokenClassifier | null = null;
  let loadPromise: Promise<TokenClassifier | null> | null = null;
  // A genuinely-MISSING package latches HARD for the process lifetime: re-running
  // a failing dynamic import() re-runs ESM resolution and retains per-attempt V8
  // bookkeeping, so a per-segment retry storm leaks heap + floods err.log (the
  // 2026-06-11 OOM — see ../optional-module.ts).
  let loadFailedPermanently = false;
  // A TRANSIENT failure (e.g. the model weights racing their lazy first-run
  // download) must NOT latch. Instead we back off until this timestamp, then
  // retry — so the redactor SELF-HEALS once the model lands, no restart needed.
  // The gate also means at most one real load attempt per cooldown window, so a
  // persistent transient failure still can't storm the loader.
  let retryNotBefore = 0;
  let warnedUnavailable = false;
  let warnedInferenceError = false;

  async function loadPipeline(): Promise<TokenClassifier | null> {
    if (cached) return cached;
    if (loadFailedPermanently) return null; // package absent — never retry
    if (retryNotBefore && now() < retryNotBefore) return null; // in post-failure cooldown
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
          retryNotBefore = 0;
          return p;
        } catch (err) {
          loadPromise = null;
          const missing = isMissingOptionalModuleError(err);
          if (missing)
            loadFailedPermanently = true; // package won't appear mid-process — latch
          else retryNotBefore = now() + retryCooldownMs; // transient — retry after cooldown
          if (!warnedUnavailable) {
            warnedUnavailable = true;
            console.warn(
              missing
                ? "[privacy-filter] adapter unavailable — install @huggingface/transformers in the consuming package to enable model-based redaction. Falling back to pass-through (warning once)."
                : `[privacy-filter] NER model load failed — retrying after ${Math.round(
                    retryCooldownMs / 1000,
                  )}s cooldown; pass-through until then (warning once).`,
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

    // Decode entities from the BIO tags: a `B-` tag or a type change starts a
    // new entity, an `I-` of the same type extends it. Transformers.js
    // returns one entry per subword (O tokens already dropped); we keep each
    // token's word + character offsets so redaction can use whichever the
    // model actually provides.
    type NerToken = { word: string; start?: number; end?: number };
    const spans: Array<{ type: string; tokens: NerToken[] }> = [];
    for (const t of tokens) {
      const ent = t.entity ?? "";
      if (!ent || ent === "O" || ent === "0") continue;
      const type = ent.replace(/^[BILUE]-/, "").toUpperCase();
      const last = spans[spans.length - 1];
      if (last && last.type === type && !/^B-/i.test(ent)) last.tokens.push(t);
      else spans.push({ type, tokens: [t] });
    }

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
      // Precise: redact exactly the detected span — an identical name
      // elsewhere in the text is left untouched. Right-to-left so each
      // splice keeps the remaining offsets valid.
      const ranges = spans
        .map((s) => ({
          type: s.type,
          start: Math.min(...s.tokens.map((t) => t.start as number)),
          end: Math.max(...s.tokens.map((t) => t.end as number)),
        }))
        .sort((a, b) => b.start - a.start);
      for (const r of ranges) {
        bump(r.type);
        out = `${out.slice(0, r.start)}[REDACTED:${r.type}]${out.slice(r.end)}`;
      }
    } else {
      // No offsets: reconstruct each entity's surface text and redact every
      // WORD-BOUNDARY occurrence (Unicode "not flanked by a letter/digit").
      // Trades precision for recall — an identical name elsewhere is also
      // redacted — but word boundaries (NOT raw substring) keep "Mark" from
      // mangling "Marketing". Longest surface first so "Ada Lovelace" is
      // redacted before a standalone "Ada".
      const surfaces = new Map<string, string>();
      for (const s of spans) {
        const surface = reconstructSurface(s.tokens.map((t) => t.word));
        if (surface && !surfaces.has(surface)) surfaces.set(surface, s.type);
      }
      for (const [surface, type] of [...surfaces].sort((a, b) => b[0].length - a[0].length)) {
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

/**
 * Probe a redactor with a sentinel PERSON entity to prove the on-device NER
 * model is actually LIVE — not a silent pass-through.
 * {@link createPrivacyFilterRedactor} degrades to a no-op when its
 * `@huggingface/transformers` peer dep is missing or the model hasn't finished
 * downloading; on the fail-closed egress paths (cloud/self-hosted) that would
 * let names/orgs the regex floor can't catch leave the box unredacted. The
 * sentinel name is NOT a regex-floor target, so a scrubbed result proves the NER
 * layer ran. Never throws — a failure answers `false` (treat as unavailable).
 *
 * Typed structurally (only `.text` is read) so it accepts any redactor — the
 * strict-counts {@link Redactor} here and the pipeline's looser one alike.
 */
export async function nerRedactorActive(
  redact: (text: string) => Promise<{ text: string }>,
): Promise<boolean> {
  const sentinel = "Escalate the incident to Katherine Johnson at Globex Corporation.";
  try {
    const out = (await redact(sentinel)).text;
    return !out.includes("Katherine Johnson");
  } catch {
    return false;
  }
}
