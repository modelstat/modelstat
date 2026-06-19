import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { defineConfig } from "tsup";

// Bake the package version into the bundle at build time. The
// previous approach — runtime parent-walk for package.json — broke
// the moment we copied dist/cli.mjs to ~/.modelstat/bin/ without a
// sibling package.json (which is always). Three modules used to
// hardcode DAEMON_VERSION as a string constant and inevitably
// drifted apart across releases (cli.ts said "0.0.1", scan.ts said
// "0.0.23", daemon.ts walked the disk). Now they all use the same
// `__MODELSTAT_VERSION__` macro that esbuild substitutes at build
// time.
const __pkgPath = join(dirname(fileURLToPath(import.meta.url)), "package.json");
const __pkgVersion = JSON.parse(readFileSync(__pkgPath, "utf8")).version;

/**
 * Bundles the daemon CLI into a single self-contained `dist/cli.mjs`
 * with a Node shebang.
 *
 * `installBundle()` in service.ts copies the .mjs to
 * `~/.modelstat/bin/modelstat.mjs` and stages ONLY the native
 * summariser packages (`node-llama-cpp` + its per-platform prebuilt
 * binary) into a sibling `node_modules` via `installNativeRuntime()`.
 * Everything else must therefore be inlined — `noExternal` grabs every
 * dep declared in package.json plus every workspace package. Only
 * `node:*` builtins (resolved by Node itself) and those native packages
 * (they ship `.node` files that can't be inlined into one `.mjs`) stay
 * external.
 *
 * If you add another native module (something that ships .node
 * binaries), list it under `external` here AND carry it as a real
 * dependency so `installNativeRuntime()`'s `npm install` pulls it into
 * the sibling node_modules. Keep pure-JS deps inlined so that
 * node_modules stays as small as possible.
 */
export default defineConfig({
  entry: { cli: "src/cli.ts" },
  format: ["esm"],
  platform: "node",
  target: "node20",
  outDir: "dist",
  outExtension: () => ({ js: ".mjs" }),
  clean: true,
  minify: false,
  sourcemap: true,
  // Bundle EVERYTHING (workspace deps + npm deps) EXCEPT:
  //   • node:* builtins (Node resolves those itself)
  //   • native-binary packages (node-llama-cpp + its per-platform
  //     prebuilt-binary subpackages, plus the @reflink/* native
  //     helper). These ship .node files that can't be inlined into
  //     a single .mjs; they're real `dependencies` in package.json
  //     so `npm install` lays them down beside the bundle and the
  //     dynamic `import("node-llama-cpp")` resolves at runtime.
  // tsup applies noExternal AFTER external, so the negative
  // lookahead has to also exclude the native packages — otherwise
  // they get force-inlined and esbuild chokes on the .node files.
  noExternal: [/^(?!node:|node-llama-cpp|@node-llama-cpp\/|@reflink\/)/],
  external: ["node-llama-cpp", /^@node-llama-cpp\//, /^@reflink\//],
  // ESM output + bundled CJS deps (undici, chokidar) can't use bare
  // `require()` — tsup's polyfill chokes on Node builtins like
  // `require("assert")`. Hand a real `createRequire` down via banner
  // so any CJS-compiled dep that calls require() at runtime resolves
  // through Node's own loader. Standard recipe; see
  // https://github.com/egoist/tsup/issues/927 + /pull/1000.
  //
  // The banner also runs a Node version preflight BEFORE the bundled
  // body executes. undici 7.x references `globalThis.File` (Node 20+)
  // at module-evaluation time, so on Node 18 the bundle crashes with
  // `ReferenceError: File is not defined` before anything we own can
  // print a useful message. npm's engines field is only a warning, so
  // npx happily installs and then explodes. The check below exits
  // fast with an actionable error.
  banner: {
    js: [
      "#!/usr/bin/env node",
      "{",
      "  const [__msMaj, __msMin] = process.versions.node.split('.').map(Number);",
      "  if (__msMaj < 20 || (__msMaj === 20 && __msMin < 18)) {",
      "    process.stderr.write(`modelstat requires Node \\u2265 20.18 (you have ${process.version}).\\n`);",
      "    process.stderr.write('Install Node 20+: https://nodejs.org\\n');",
      "    process.stderr.write('Debian/Ubuntu: curl -fsSL https://deb.nodesource.com/setup_20.x | sudo -E bash - && sudo apt-get install -y nodejs\\n');",
      "    process.exit(1);",
      "  }",
      "}",
      'import { createRequire as __modelstatCR } from "node:module";',
      "const require = __modelstatCR(import.meta.url);",
    ].join("\n"),
  },
  shims: false,
  splitting: false,
  // esbuild macro substitution. Source code references
  // `__MODELSTAT_VERSION__` and reads it as a string literal at
  // bundle time — no runtime file-walk, no drift across modules.
  define: {
    __MODELSTAT_VERSION__: JSON.stringify(`daemon-${__pkgVersion}`),
  },
});
