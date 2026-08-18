/**
 * Regenerate the golden fixtures that the LIVE TypeScript owns.
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
import { generator as misc } from "./gen-misc.mts";
import type { Generator } from "./lib.mts";

// This generator covers only the families that still have a live TypeScript
// implementation (`packages/core`, `packages/daemon-core` — both shipped by the
// Chrome extension and the MCP server). Two families are owned elsewhere:
//   * parser goldens are Rust-generated since SPEC 0005 (the Rust parsers
//     deliberately supersede the retired TS port) — see the REGEN_GOLDENS test
//     in daemon/crates/modelstat-parsers/tests/golden_parsers.rs;
//   * device.json / shell_executable.json / tool_name.json are FROZEN vectors
//     whose TS side is deleted — see the header of gen-ids-tooling.mts.
const GENERATORS: Generator[] = [idsTooling, redaction, wire, misc];

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
