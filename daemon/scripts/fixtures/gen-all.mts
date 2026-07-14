/**
 * Regenerate every golden fixture from the TypeScript source of truth.
 *
 *   pnpm -C daemon fixtures            # or: npx tsx daemon/scripts/fixtures/gen-all.mts
 *
 * Output is deterministic; CI runs this and gates on `git diff --exit-code` so a
 * TS change that would move a fixture (i.e. break wire/id/redaction parity)
 * fails loudly instead of silently drifting from the Rust port.
 */
import { generator as idsTooling } from "./gen-ids-tooling.mts";
import { generator as redaction } from "./gen-redaction.mts";
import { generator as wire } from "./gen-wire.mts";
import { generator as parsers } from "./gen-parsers.mts";
import { generator as misc } from "./gen-misc.mts";
import type { Generator } from "./lib.mts";

const GENERATORS: Generator[] = [idsTooling, redaction, wire, parsers, misc];

async function main(): Promise<void> {
  for (const gen of GENERATORS) {
    // eslint-disable-next-line no-console
    console.log(`\n▸ ${gen.category}`);
    await gen.run();
  }
  // eslint-disable-next-line no-console
  console.log("\n✓ all golden fixtures regenerated");
}

main().catch((err) => {
  // eslint-disable-next-line no-console
  console.error(err);
  process.exit(1);
});
