/**
 * Bundle the session-insights View (src/view/main.ts + style.css) into a single
 * self-contained HTML document at dist/session-insights.html.
 *
 * The bridge serves this as the `ui://modelstat/session-insights` MCP-UI
 * resource (see src/widget.ts). It must be self-contained — the host loads it
 * into a sandboxed iframe with no same-origin server, so the JS and CSS are
 * inlined (no external <script>/<link>). The View talks to the host only over
 * postMessage (ext-apps `App`), so no network origins are needed in the CSP.
 *
 * Runs after `tsup` in the `build` script (tsup --clean wipes dist first).
 */
import * as esbuild from "esbuild";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const pkgDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const viewDir = join(pkgDir, "src", "view");
const outFile = join(pkgDir, "dist", "session-insights.html");

const result = await esbuild.build({
  entryPoints: [join(viewDir, "main.ts")],
  bundle: true,
  format: "iife",
  platform: "browser",
  target: ["es2020"],
  minify: true,
  write: false,
  legalComments: "none",
  // ext-apps pulls in the MCP SDK + zod, which probe process.env.NODE_ENV.
  define: { "process.env.NODE_ENV": '"production"' },
  banner: { js: "globalThis.process||={env:{}};" },
});

const js = result.outputFiles[0].text;
const css = readFileSync(join(viewDir, "style.css"), "utf8");

const html = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<title>modelstat · session insights</title>
<style>${css}</style>
</head>
<body>
<div id="root" aria-live="polite"></div>
<script>${js}</script>
</body>
</html>
`;

mkdirSync(dirname(outFile), { recursive: true });
writeFileSync(outFile, html);
process.stderr.write(
  `build-view: wrote dist/session-insights.html (${(html.length / 1024).toFixed(0)} KB)\n`,
);
