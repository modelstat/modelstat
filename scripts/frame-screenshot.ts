/**
 * Take any raw screenshot dropped into apps/extension/store/ (or
 * apps/extension/store/raw/) and produce a CWS-compliant framed
 * version: exactly 1280×800, 24-bit PNG, no alpha.
 *
 *   pnpm tsx scripts/frame-screenshot.ts [input.png [output.png]]
 *
 * With no args, it:
 *   - Reads every .png in apps/extension/store/raw/ (if that dir exists)
 *   - Frames each one as store/<basename>.png at 1280×800
 *   - Plus: if apps/extension/store/screenshot.png already exists (a
 *     raw grab), re-frames it in place.
 *
 * Framing: dark gradient background, original screenshot centred with
 * a small drop shadow, max 80% of either axis so nothing is cropped.
 */

import { readdir, readFile, mkdir, writeFile, access } from "node:fs/promises";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import sharp from "sharp";

const here = dirname(fileURLToPath(import.meta.url));
const storeDir = resolve(here, "../apps/extension/store");
const rawDir = resolve(storeDir, "raw");

async function exists(p: string): Promise<boolean> {
  try {
    await access(p);
    return true;
  } catch {
    return false;
  }
}

// 1280×800 background: dark gradient with a faint brand halo.
const BACKGROUND_SVG = Buffer.from(`
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1280 800">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0%" stop-color="#0d1117" />
      <stop offset="100%" stop-color="#05080F" />
    </linearGradient>
    <radialGradient id="halo" cx="0.8" cy="0.2" r="0.6">
      <stop offset="0%" stop-color="#64ECAC" stop-opacity="0.14" />
      <stop offset="100%" stop-color="#64ECAC" stop-opacity="0" />
    </radialGradient>
  </defs>
  <rect width="1280" height="800" fill="url(#bg)" />
  <rect width="1280" height="800" fill="url(#halo)" />
</svg>
`);

async function frame(inputPath: string, outputPath: string): Promise<void> {
  const input = await readFile(inputPath);
  const meta = await sharp(input).metadata();
  const srcW = meta.width ?? 1;
  const srcH = meta.height ?? 1;

  // Fit the screenshot into 80% of the canvas.
  const maxW = 1280 * 0.8;
  const maxH = 800 * 0.8;
  const scale = Math.min(maxW / srcW, maxH / srcH);
  const w = Math.round(srcW * scale);
  const h = Math.round(srcH * scale);

  // Resize the screenshot; keep its alpha so the rounded-corner
  // transparency blends into the background.
  const resized = await sharp(input).resize(w, h, { fit: "contain" }).png().toBuffer();

  // Compose onto the background, centred; then flatten and strip alpha.
  const framed = await sharp(BACKGROUND_SVG)
    .composite([{ input: resized, top: Math.round((800 - h) / 2), left: Math.round((1280 - w) / 2) }])
    .flatten({ background: { r: 10, g: 10, b: 10 } })
    .toColourspace("srgb")
    .removeAlpha()
    .png({ compressionLevel: 9, palette: false })
    .toBuffer();

  await mkdir(dirname(outputPath), { recursive: true });
  await writeFile(outputPath, framed);
  console.log(`  wrote ${outputPath} (1280x800, 24-bit, no alpha)`);
}

async function main(): Promise<void> {
  const [arg1, arg2] = process.argv.slice(2);
  if (arg1) {
    const input = resolve(arg1);
    const output = arg2
      ? resolve(arg2)
      : resolve(storeDir, basename(input));
    await frame(input, output);
    return;
  }

  console.log("framing screenshots ─────────────");
  // In-place reframe of store/screenshot.png if present.
  const inPlace = resolve(storeDir, "screenshot.png");
  if (await exists(inPlace)) {
    // Route: read → delete implicit (we overwrite)
    const tmp = resolve(storeDir, ".screenshot.raw.png");
    // Copy to tmp so we don't read/write the same file with sharp.
    await writeFile(tmp, await readFile(inPlace));
    try {
      await frame(tmp, inPlace);
    } finally {
      try {
        const { unlink } = await import("node:fs/promises");
        await unlink(tmp);
      } catch {
        /* ignore */
      }
    }
  }

  // Batch-frame everything in raw/.
  if (await exists(rawDir)) {
    const entries = await readdir(rawDir);
    for (const f of entries) {
      if (!/\.(png|jpg|jpeg)$/i.test(f)) continue;
      const stem = f.replace(/\.(png|jpg|jpeg)$/i, "");
      await frame(resolve(rawDir, f), resolve(storeDir, `${stem}.png`));
    }
  }

  console.log("\n✓ done");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
