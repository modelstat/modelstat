/**
 * Build modelstat.mcpb — a Claude Desktop (MCPB) bundle of the modelstat MCP
 * server, for one-click install via Settings → Extensions.
 *
 * The bundle is a zip of:
 *   manifest.json      (MCPB 0.3 — declares a node server)
 *   server/index.mjs   (the @modelstat/mcp server, esbuild-bundled, deps inlined)
 *   icon.png
 *
 * The server is the same thin bridge published as @modelstat/mcp: it forwards
 * to the backend and fetches its tool catalog live, so a pinned bundle still
 * gets new tools without a re-pack.
 *
 *   pnpm build:mcpb   →   packages/mcp/modelstat.mcpb
 *
 * Hosting: both repos are private, so the artifact is served from our own
 * domain — copy the built modelstat.mcpb into core `apps/web/public/` (served
 * at https://modelstat.ai/modelstat.mcpb). Re-run + re-copy when the bridge's
 * core forwarding/auth changes (rare; the catalog is live, so tool changes
 * don't need a re-pack).
 */
import { build } from "esbuild";
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pkgRoot = join(here, "..");
const pkg = JSON.parse(readFileSync(join(pkgRoot, "package.json"), "utf8"));

const stage = join(pkgRoot, "mcpb-dist");
rmSync(stage, { recursive: true, force: true });
mkdirSync(join(stage, "server"), { recursive: true });

// 1. Self-contained ESM bundle — deps inlined, node builtins external.
await build({
  entryPoints: [join(pkgRoot, "src/index.ts")],
  bundle: true,
  platform: "node",
  format: "esm",
  target: "node20",
  outfile: join(stage, "server", "index.mjs"),
  minify: true,
  legalComments: "none",
});

// 2. Manifest (MCPB 0.3), version synced from package.json.
const manifest = {
  manifest_version: "0.3",
  name: "modelstat",
  display_name: "modelstat",
  version: pkg.version,
  description: "Ask any AI tool about your token spend — modelstat analytics, in Claude Desktop.",
  long_description:
    "modelstat tracks what your AI coding tools cost and attributes the spend to projects, models and people. This extension lets you ask Claude Desktop about it directly — \"what did I spend this week?\", \"which project is most expensive?\" — answered from your modelstat account. On first use it opens a browser tab to connect to your account; if the modelstat daemon is installed it reuses that automatically.",
  author: { name: "modelstat", url: "https://modelstat.ai" },
  homepage: "https://modelstat.ai",
  documentation: "https://modelstat.ai/developers",
  support: "https://github.com/modelstat/modelstat/issues",
  icon: "icon.png",
  license: "MIT",
  keywords: ["ai", "tokens", "cost", "observability", "spend", "modelstat"],
  server: {
    type: "node",
    entry_point: "server/index.mjs",
    mcp_config: {
      command: "node",
      args: ["${__dirname}/server/index.mjs"],
    },
  },
};
writeFileSync(join(stage, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);

// 3. Icon (committed alongside this script).
const icon = join(pkgRoot, "mcpb", "icon.png");
if (existsSync(icon)) copyFileSync(icon, join(stage, "icon.png"));
else throw new Error(`icon not found: ${icon}`);

// 4. Zip → modelstat.mcpb (an .mcpb IS a zip).
const out = join(pkgRoot, "modelstat.mcpb");
rmSync(out, { force: true });
execFileSync("zip", ["-r", "-X", "-q", out, "manifest.json", "server", "icon.png"], {
  cwd: stage,
  stdio: "inherit",
});
console.log(`built ${out} (modelstat ${pkg.version})`);
