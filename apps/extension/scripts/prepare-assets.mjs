#!/usr/bin/env node
/**
 * Prebuild step for apps/extension. Copies the tiktoken vocab JSONs
 * (and the wasm blob) out of node_modules into public/assets/ so vite
 * includes them in the built extension bundle.
 *
 * Run automatically via the `build` script in package.json, but you
 * can invoke it directly with:
 *   node scripts/prepare-assets.mjs
 */
import { cp, mkdir, access } from "node:fs/promises";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const extRoot = resolve(here, "..");
const tiktokenDir = resolve(extRoot, "node_modules/tiktoken");

async function exists(p) {
  try {
    await access(p);
    return true;
  } catch {
    return false;
  }
}

async function copy(src, dst, label) {
  if (!(await exists(src))) {
    console.warn(`  skip ${label}: ${src} not found`);
    return;
  }
  await mkdir(dirname(dst), { recursive: true });
  await cp(src, dst);
  console.log(`  copied ${label}`);
}

console.log("extension prepare-assets ─────────────");

// Bundled adapter configs — the extension reads these on boot as the
// last-known-good fallback when the modelstat API is unreachable or
// returns invalid signatures. Copy from apps/extension/adapters/ into
// public/adapters/ so vite includes them in the dist.
{
  const src = resolve(extRoot, "adapters");
  const dst = resolve(extRoot, "public/adapters");
  const { readdir } = await import("node:fs/promises");
  const files = await readdir(src).catch(() => []);
  for (const f of files) {
    if (!f.endsWith(".json")) continue;
    await copy(resolve(src, f), resolve(dst, f), `adapters/${f}`);
  }
}

await copy(
  resolve(tiktokenDir, "encoders/o200k_base.json"),
  resolve(extRoot, "public/assets/tiktoken/o200k_base.json"),
  "tiktoken/o200k_base.json",
);
await copy(
  resolve(tiktokenDir, "encoders/cl100k_base.json"),
  resolve(extRoot, "public/assets/tiktoken/cl100k_base.json"),
  "tiktoken/cl100k_base.json",
);
await copy(
  resolve(tiktokenDir, "tiktoken_bg.wasm"),
  resolve(extRoot, "public/tiktoken_bg.wasm"),
  "tiktoken_bg.wasm",
);
console.log("✓ assets ready\n");
