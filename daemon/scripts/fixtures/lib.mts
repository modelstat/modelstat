/**
 * Shared helpers for the golden-fixture generators.
 *
 * These scripts run against the EXISTING TypeScript implementation (the source
 * of truth while it lives) and write deterministic JSON fixtures the Rust crates
 * assert byte-parity against. Determinism is a hard requirement: no Date.now(),
 * no randomness, fixed inputs — so `pnpm -C daemon fixtures` regenerates
 * byte-identical output and CI can gate on `git diff --exit-code`.
 *
 * See daemon/crates/modelstat-wire/tests/golden/README.md for the catalogue.
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/** Absolute path to daemon/crates/modelstat-wire/tests/golden, resolved from
 * this file so the generators work regardless of cwd. */
export const GOLDEN_DIR = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "crates",
  "modelstat-wire",
  "tests",
  "golden",
);

/** Write `value` as pretty JSON (2-space, trailing newline) under GOLDEN_DIR. */
export function writeGolden(relPath: string, value: unknown): void {
  const dest = join(GOLDEN_DIR, relPath);
  mkdirSync(dirname(dest), { recursive: true });
  writeFileSync(dest, `${JSON.stringify(value, null, 2)}\n`);
  // eslint-disable-next-line no-console
  console.log(`  ✓ ${relPath}`);
}

/** A generator module: a name + the function that writes its fixtures. */
export interface Generator {
  readonly category: string;
  run(): void | Promise<void>;
}
