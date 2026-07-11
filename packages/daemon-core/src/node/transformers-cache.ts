/**
 * A single, STABLE, SHARED on-disk cache dir for Transformers.js models.
 *
 * Why this exists: Transformers.js caches downloaded models under
 * `<@huggingface/transformers package>/.cache/` — i.e. RELATIVE to whichever
 * copy of the package the running process resolves (see its `env.js`
 * DEFAULT_CACHE_DIR). But the two processes that touch these models resolve
 * DIFFERENT copies:
 *   - `modelstat connect` runs from the global npm install, and
 *   - the daemon runs from the staged runtime at `~/.modelstat/bin`.
 * So a model warmed by `connect` would be invisible to the daemon, and every
 * auto-update — which re-stages the runtime and wipes its `node_modules/.../
 * .cache` — would waste a full re-download.
 *
 * Fix: pin `env.cacheDir` to a fixed path BOTH processes compute identically,
 * mirroring the llama model dir (`defaultLlamaConfig().modelsDir`). Now there is
 * ONE cache, shared across processes and surviving upgrades. This is the NER/PII
 * redactor (`../redact/privacy-filter.ts`) and the embedder
 * (`./transformersjs-embed.ts`) counterpart to the llama `modelsDir`.
 */
import { mkdir } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

/** The shared Transformers.js cache dir: `~/.modelstat/models/hf` (honours the
 * same `MODELSTAT_MODELS_DIR` override the llama model dir uses). Both the
 * connect CLI and the daemon compute this identically, so a warm from one is a
 * cache hit for the other. */
export function transformersCacheDir(): string {
  const env =
    (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env ?? {};
  const modelsDir = env.MODELSTAT_MODELS_DIR ?? join(homedir(), ".modelstat", "models");
  return join(modelsDir, "hf");
}

let applied = false;

/**
 * Point Transformers.js at the shared on-disk cache ({@link transformersCacheDir}),
 * ONCE per process and BEFORE the first `pipeline()` call. Idempotent and
 * best-effort: `@huggingface/transformers` is an OPTIONAL peer dep, so if it
 * isn't installed there is nothing to configure and this quietly no-ops (the
 * redactor/embedder then fall through to their pass-through behaviour anyway).
 *
 * Must be awaited before any redactor/embedder loads a model — the daemon calls
 * it at its pipeline choke points and the connect CLI calls it inside
 * {@link ensureNerModel}, so both the warm and the read land in the same dir.
 */
export async function applyTransformersCacheDir(): Promise<void> {
  if (applied) return;
  applied = true;
  try {
    const dir = transformersCacheDir();
    await mkdir(dir, { recursive: true });
    // Indirect import via a string VARIABLE (not a literal) so TypeScript doesn't
    // statically resolve @huggingface/transformers — it's an OPTIONAL peer dep,
    // matching ./transformersjs-embed.ts and ../redact/privacy-filter.ts.
    const importModule = (id: string): Promise<unknown> => import(/* @vite-ignore */ id);
    const tjs = (await importModule("@huggingface/transformers")) as {
      env?: { cacheDir?: string; useFSCache?: boolean };
    };
    if (tjs.env) {
      tjs.env.cacheDir = dir;
      tjs.env.useFSCache = true;
    }
  } catch {
    // Optional dep absent or env not settable — nothing to cache, carry on.
  }
}
