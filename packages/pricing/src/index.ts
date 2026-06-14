/**
 * Pricing package. Loads the YAML files under /prices at startup and
 * exposes `priceFor(provider, model, at?)` (the active PricePoint — rates
 * are USD per MILLION tokens per class) plus `computeCostUsd(...)` (USD for
 * a turn's tokens). Call `loadPrices()` again to hot-reload after the YAML
 * changes; it clears and rebuilds the indexes in place.
 */
import { readFile, readdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parse } from "yaml";
import type { Provider } from "@modelstat/core";

const __dirname = dirname(fileURLToPath(import.meta.url));

export interface PricePoint {
  effective_date: string; // YYYY-MM-DD
  input: number;
  output: number;
  cache_read?: number;
  cache_write?: number;
  reasoning?: number;
  notes?: string;
}

export interface ModelPrice {
  id: string;
  aliases?: string[];
  history: PricePoint[];
}

interface ProviderFile {
  provider: Provider;
  models: ModelPrice[];
}

/** model id + alias → canonical id */
const aliasIndex = new Map<string, string>();
/** canonical id → full model */
const modelIndex = new Map<string, { provider: Provider; model: ModelPrice }>();
let loaded = false;

async function loadDir(dir: string): Promise<ProviderFile[]> {
  const files = (await readdir(dir)).filter((f) => f.endsWith(".yaml") || f.endsWith(".yml"));
  const out: ProviderFile[] = [];
  for (const f of files) {
    const raw = await readFile(join(dir, f), "utf8");
    const pf = parse(raw) as ProviderFile;
    if (!pf?.provider || !Array.isArray(pf.models)) {
      console.warn(`[pricing] skipping malformed price file: ${f}`);
      continue;
    }
    out.push(pf);
  }
  return out;
}

export async function loadPrices(dir?: string): Promise<void> {
  const root = dir ?? resolve(__dirname, "../../../prices");
  const providers = await loadDir(root);

  aliasIndex.clear();
  modelIndex.clear();

  for (const pf of providers) {
    for (const m of pf.models) {
      const key = `${pf.provider}::${m.id}`;
      modelIndex.set(key, { provider: pf.provider, model: m });
      aliasIndex.set(`${pf.provider}::${m.id}`, key);
      for (const a of m.aliases ?? []) aliasIndex.set(`${pf.provider}::${a}`, key);
    }
  }
  loaded = true;
}

function ensureLoaded(): void {
  if (!loaded) {
    throw new Error("@modelstat/pricing: call loadPrices() during boot");
  }
}

function pickPricePoint(m: ModelPrice, at: Date): PricePoint | undefined {
  const atStr = at.toISOString().slice(0, 10);
  let best: PricePoint | undefined;
  for (const p of m.history) {
    if (p.effective_date <= atStr) {
      if (!best || p.effective_date > best.effective_date) best = p;
    }
  }
  return best ?? m.history[0];
}

export interface CostInputs {
  input?: number;
  output?: number;
  cache_creation?: number;
  cache_read?: number;
  reasoning?: number;
}

/**
 * Look up the active price point for a model at a date (defaults to now).
 * Resolves aliases and strips `[1m]`-style suffixes the same way
 * {@link computeCostUsd} does, then picks the most recent history entry
 * effective on or before `at`. Returns null for an unknown model or one
 * with no price history. Rates on the returned PricePoint are USD per
 * million tokens.
 */
export function priceFor(
  provider: Provider,
  model: string | null | undefined,
  at: Date = new Date(),
): PricePoint | null {
  ensureLoaded();
  if (!model) return null;
  const norm = normaliseModelId(provider, model);
  const canonical = aliasIndex.get(`${provider}::${norm}`);
  const entry = canonical ? modelIndex.get(canonical) : undefined;
  if (!entry) return null;
  return pickPricePoint(entry.model, at) ?? null;
}

/** USD cost for a single turn's tokens. Unknown model → 0, logs once. */
const warned = new Set<string>();
export function computeCostUsd(
  provider: Provider,
  model: string | null | undefined,
  tokens: CostInputs,
  at: Date = new Date(),
): number {
  ensureLoaded();
  if (!model) return 0;
  const p = priceFor(provider, model, at);
  if (!p) {
    const key = `${provider}:${normaliseModelId(provider, model)}`;
    if (!warned.has(key)) {
      warned.add(key);
      console.warn(`[pricing] unknown model ${key} — cost will be $0`);
    }
    return 0;
  }
  const perMtok = (usd: number | undefined, n: number): number =>
    usd && n ? (usd * n) / 1_000_000 : 0;
  return (
    perMtok(p.input, tokens.input ?? 0) +
    perMtok(p.output, tokens.output ?? 0) +
    perMtok(p.cache_read, tokens.cache_read ?? 0) +
    perMtok(p.cache_write, tokens.cache_creation ?? 0) +
    perMtok(p.reasoning ?? p.output, tokens.reasoning ?? 0)
  );
}

/** Canonicalise a vendor model string: strip suffixes like [1m], trim
 * trailing date stamps if we don't know them, lower-case. */
export function normaliseModelId(_provider: Provider, raw: string): string {
  let s = raw.trim();
  // Strip bracket suffixes: claude-opus-4-6[1m] → claude-opus-4-6
  s = s.replace(/\[[^\]]+\]$/, "");
  return s;
}

/** List every known (provider, canonical_model_id) tuple — useful for
 * admin UI and debug endpoints. */
export function listKnownModels(): Array<{ provider: Provider; model: string }> {
  ensureLoaded();
  const out: Array<{ provider: Provider; model: string }> = [];
  for (const { provider, model } of modelIndex.values()) {
    out.push({ provider, model: model.id });
  }
  return out;
}
