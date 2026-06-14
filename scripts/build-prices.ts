/**
 * Emit a browser-friendly prices.json from packages/pricing's YAML
 * sources. The extension ships this JSON as a bundled asset and uses
 * it to compute $-equivalent costs client-side.
 *
 *   pnpm tsx scripts/build-prices.ts [--prices <dir>]
 *
 * The YAML source of truth lives in a private upstream repo under
 * /prices/*.yaml — when running in a sibling-checkout layout the
 * default resolves to ../prices-upstream. Override with --prices
 * or PRICES_DIR for other layouts (CI artefact dir, single-repo
 * dev checkout, etc.).
 */

import { existsSync } from "node:fs";
import { mkdir, writeFile } from "node:fs/promises";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { loadPrices, listKnownModels, computeCostUsd } from "@modelstat/pricing";

const here = dirname(fileURLToPath(import.meta.url));
const OUT = resolve(here, "../apps/extension/public/assets/prices.json");

function resolvePricesDir(): string {
  const args = process.argv.slice(2);
  const i = args.indexOf("--prices");
  if (i >= 0 && args[i + 1]) return resolve(process.cwd(), args[i + 1]!);
  if (process.env.PRICES_DIR) return resolve(process.cwd(), process.env.PRICES_DIR);
  // Default to a sibling upstream checkout. Works when this repo and
  // the private upstream live next to each other.
  const sibling = resolve(here, "../prices-upstream");
  if (existsSync(sibling)) return sibling;
  // Fallback to a bundled ./prices if the operator vendored them in.
  return resolve(here, "../prices");
}

type Rate = {
  input: number;
  output: number;
  cache_creation?: number;
  cache_read?: number;
  reasoning?: number;
};

async function main(): Promise<void> {
  const pricesDir = resolvePricesDir();
  console.log(`▶ loading prices from ${pricesDir}`);
  await loadPrices(pricesDir);
  const known = listKnownModels();
  const table: Record<string, Record<string, Rate>> = {};
  for (const { provider, model } of known) {
    // computeCostUsd takes a usage spec; probe per-kind with 1M tokens
    // to derive $/M rates. This keeps the transform trivial and honest
    // even if the underlying schema changes.
    const input = computeCostUsd(provider, model, {
      input: 1_000_000,
      output: 0,
      cache_creation: 0,
      cache_read: 0,
      reasoning: 0,
    });
    const output = computeCostUsd(provider, model, {
      input: 0,
      output: 1_000_000,
      cache_creation: 0,
      cache_read: 0,
      reasoning: 0,
    });
    const cacheCreation = computeCostUsd(provider, model, {
      input: 0,
      output: 0,
      cache_creation: 1_000_000,
      cache_read: 0,
      reasoning: 0,
    });
    const cacheRead = computeCostUsd(provider, model, {
      input: 0,
      output: 0,
      cache_creation: 0,
      cache_read: 1_000_000,
      reasoning: 0,
    });
    const reasoning = computeCostUsd(provider, model, {
      input: 0,
      output: 0,
      cache_creation: 0,
      cache_read: 0,
      reasoning: 1_000_000,
    });
    if (!(provider in table)) table[provider] = {};
    const entry: Rate = { input, output };
    if (cacheCreation && cacheCreation !== input) entry.cache_creation = cacheCreation;
    if (cacheRead && cacheRead !== input) entry.cache_read = cacheRead;
    if (reasoning && reasoning !== output) entry.reasoning = reasoning;
    table[provider]![model] = entry;
  }
  await mkdir(dirname(OUT), { recursive: true });
  await writeFile(OUT, JSON.stringify(table, null, 2), "utf8");
  console.log(`wrote prices.json (${Object.keys(table).length} providers, ${known.length} models) → ${OUT}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
