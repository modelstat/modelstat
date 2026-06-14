/**
 * Pricing table for the extension. v1 ships a small bundled JSON emitted
 * by scripts/build-prices.ts from packages/pricing's YAML sources. Later
 * iterations can fetch an updated table over the adapter-manifest
 * channel (same signed distribution).
 */

import { createLogger } from "@/common/logger.js";

const log = createLogger("pricing");

type ModelRate = {
  input: number; // USD per million tokens
  output: number;
  cache_creation?: number;
  cache_read?: number;
  reasoning?: number;
};

type PriceTable = Record<string, Record<string, ModelRate>>;

let tablePromise: Promise<PriceTable> | null = null;

async function loadTable(): Promise<PriceTable> {
  if (tablePromise) return tablePromise;
  tablePromise = (async () => {
    try {
      const url = chrome.runtime.getURL("assets/prices.json");
      const res = await fetch(url);
      return (await res.json()) as PriceTable;
    } catch (e) {
      log.warn("prices.json missing — costs will be zero until build:prices runs", e);
      return {};
    }
  })();
  return tablePromise;
}

function bestMatch(models: Record<string, ModelRate>, model: string): ModelRate | null {
  if (models[model]) return models[model]!;
  // Fallback: longest prefix match on `-` segments (handles versioned slugs
  // like "claude-sonnet-4-6-20260101").
  const parts = model.split("-");
  for (let i = parts.length; i > 0; i--) {
    const prefix = parts.slice(0, i).join("-");
    if (models[prefix]) return models[prefix]!;
    if (models[`${prefix}-*`]) return models[`${prefix}-*`]!;
  }
  return null;
}

export async function priceFor(
  vendor: string,
  model: string | null,
  usage: {
    input: number;
    output: number;
    cache_creation: number;
    cache_read: number;
    reasoning: number;
  },
): Promise<number | null> {
  if (!model) return null;
  const table = await loadTable();
  const byVendor = table[vendor];
  if (!byVendor) return null;
  const rate = bestMatch(byVendor, model);
  if (!rate) return null;
  const cost =
    (usage.input * rate.input) / 1_000_000 +
    (usage.output * rate.output) / 1_000_000 +
    (usage.cache_creation * (rate.cache_creation ?? rate.input)) / 1_000_000 +
    (usage.cache_read * (rate.cache_read ?? rate.input)) / 1_000_000 +
    (usage.reasoning * (rate.reasoning ?? rate.output)) / 1_000_000;
  return cost;
}
