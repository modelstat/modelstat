/**
 * Exact OpenAI tokenization via tiktoken (WASM).
 *
 * We use tiktoken's `/lite/init` subpath so vite doesn't try to bundle
 * the wasm via the ESM-integration proposal (which it doesn't
 * support). Instead we load the wasm manually from the extension's
 * own origin — prepare-assets.mjs copies the wasm + vocab JSONs into
 * public/ at build time.
 */

import { init, Tiktoken } from "tiktoken/lite/init";

type Encoder = {
  encode(text: string): { length: number };
  free(): void;
};

type VocabLoader = () => Promise<{
  bpe_ranks: string;
  special_tokens: Record<string, number>;
  pat_str: string;
}>;

const loaders: Record<string, VocabLoader> = {
  o200k_base: async () => {
    const res = await fetch(chrome.runtime.getURL("assets/tiktoken/o200k_base.json"));
    return res.json() as Promise<{ bpe_ranks: string; special_tokens: Record<string, number>; pat_str: string }>;
  },
  cl100k_base: async () => {
    const res = await fetch(chrome.runtime.getURL("assets/tiktoken/cl100k_base.json"));
    return res.json() as Promise<{ bpe_ranks: string; special_tokens: Record<string, number>; pat_str: string }>;
  },
};

let initPromise: Promise<void> | null = null;
async function ensureWasmInit(): Promise<void> {
  if (initPromise) return initPromise;
  initPromise = init(async (imports) => {
    const url = chrome.runtime.getURL("tiktoken_bg.wasm");
    const res = await fetch(url);
    return WebAssembly.instantiateStreaming(res, imports);
  });
  return initPromise;
}

const cache = new Map<string, Promise<Encoder>>();

async function getEncoder(name: string): Promise<Encoder> {
  const existing = cache.get(name);
  if (existing) return existing;
  const loader = loaders[name];
  if (!loader) throw new Error(`unknown tiktoken vocab: ${name}`);
  const p = (async () => {
    await ensureWasmInit();
    const vocab = await loader();
    return new Tiktoken(vocab.bpe_ranks, vocab.special_tokens, vocab.pat_str) as unknown as Encoder;
  })();
  cache.set(name, p);
  return p;
}

export async function countTiktoken(name: string, text: string): Promise<number> {
  const enc = await getEncoder(name);
  return enc.encode(text).length;
}
