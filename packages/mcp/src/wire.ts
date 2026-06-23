/**
 * `modelstat-mcp wire` — drop the modelstat MCP server into every local AI
 * tool we can find, so the user can ask about their spend from inside them.
 *
 * Run any time:  npx -y @modelstat/mcp wire
 *
 * Cross-platform (macOS / Linux / Windows). Idempotent + non-destructive: for
 * each tool we set ONLY the single `modelstat` server entry and never touch
 * sibling servers or other settings. A tool is "detected" when its config
 * directory exists; undetected tools are skipped silently.
 *
 * This is the single source of truth for MCP wiring — install.sh / install.ps1
 * both call it, and the daemon can too. Add a tool here, every entry point gets
 * it.
 */
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir, platform as osPlatform } from "node:os";
import { dirname, join } from "node:path";

const NPX_COMMAND = "npx";
const NPX_ARGS = ["-y", "@modelstat/mcp"];

export type WireStatus = "configured" | "already" | "absent" | "skipped";
export interface WireResult {
  name: string;
  status: WireStatus;
}

type Plat = NodeJS.Platform;

/** Per-OS application-support base dir for a GUI app's config. */
function appSupport(home: string, plat: Plat, app: string): string {
  if (plat === "win32") {
    return join(process.env.APPDATA || join(home, "AppData", "Roaming"), app);
  }
  if (plat === "darwin") {
    return join(home, "Library", "Application Support", app);
  }
  return join(home, ".config", app);
}

interface JsonTarget {
  name: string;
  /** config file we merge into */
  file: string;
  /** top-level key holding the server map (mcpServers | servers | context_servers) */
  key: string;
  /** dir whose existence means the tool is installed */
  detect: string;
  /** server-entry shape — VS Code/Cursor/etc. use a plain command; Zed differs */
  shape: "command" | "zed";
}

/** The JSON/JSONC config targets, resolved for a given home + platform. */
export function jsonTargets(home: string, plat: Plat): JsonTarget[] {
  const claude = appSupport(home, plat, "Claude");
  const code = appSupport(home, plat, "Code");
  const codeInsiders = appSupport(home, plat, "Code - Insiders");
  const vscodium = appSupport(home, plat, "VSCodium");
  return [
    { name: "Claude Desktop", file: join(claude, "claude_desktop_config.json"), key: "mcpServers", detect: claude, shape: "command" },
    { name: "Cursor", file: join(home, ".cursor", "mcp.json"), key: "mcpServers", detect: join(home, ".cursor"), shape: "command" },
    // VS Code (and forks) read a dedicated user-level mcp.json with a top-level `servers` key.
    { name: "VS Code", file: join(code, "User", "mcp.json"), key: "servers", detect: code, shape: "command" },
    { name: "VS Code Insiders", file: join(codeInsiders, "User", "mcp.json"), key: "servers", detect: codeInsiders, shape: "command" },
    { name: "VSCodium", file: join(vscodium, "User", "mcp.json"), key: "servers", detect: vscodium, shape: "command" },
    { name: "Windsurf", file: join(home, ".codeium", "windsurf", "mcp_config.json"), key: "mcpServers", detect: join(home, ".codeium", "windsurf"), shape: "command" },
    { name: "Gemini CLI", file: join(home, ".gemini", "settings.json"), key: "mcpServers", detect: join(home, ".gemini"), shape: "command" },
    { name: "Zed", file: join(home, ".config", "zed", "settings.json"), key: "context_servers", detect: join(home, ".config", "zed"), shape: "zed" },
  ];
}

function serverEntry(shape: "command" | "zed"): unknown {
  if (shape === "zed") {
    return { source: "custom", command: NPX_COMMAND, args: NPX_ARGS, env: {} };
  }
  return { command: NPX_COMMAND, args: NPX_ARGS };
}

/** Merge the modelstat server into one JSON config. Returns its status. */
export function wireJsonTarget(t: JsonTarget): WireStatus {
  if (!existsSync(t.detect)) return "absent";
  let cfg: Record<string, unknown> = {};
  if (existsSync(t.file)) {
    try {
      const parsed = JSON.parse(readFileSync(t.file, "utf8")) as unknown;
      if (parsed && typeof parsed === "object") cfg = parsed as Record<string, unknown>;
    } catch {
      // malformed/empty → start fresh but keep the write going
    }
  }
  const existingBag = cfg[t.key];
  const bag: Record<string, unknown> =
    existingBag && typeof existingBag === "object" ? (existingBag as Record<string, unknown>) : {};
  const desired = serverEntry(t.shape);
  if (JSON.stringify(bag.modelstat) === JSON.stringify(desired)) return "already";
  bag.modelstat = desired;
  cfg[t.key] = bag;
  try {
    mkdirSync(dirname(t.file), { recursive: true });
    writeFileSync(t.file, `${JSON.stringify(cfg, null, 2)}\n`);
    return "configured";
  } catch {
    return "skipped";
  }
}

/** Codex uses TOML; append the [mcp_servers.modelstat] table if absent. */
export function wireCodex(home: string): WireStatus {
  const dir = join(home, ".codex");
  if (!existsSync(dir)) return "absent";
  const file = join(dir, "config.toml");
  let toml = "";
  if (existsSync(file)) {
    try {
      toml = readFileSync(file, "utf8");
    } catch {
      toml = "";
    }
  }
  if (/\[mcp_servers\.modelstat\]/.test(toml)) return "already";
  const block = `[mcp_servers.modelstat]\ncommand = "npx"\nargs = ["-y", "@modelstat/mcp"]\n`;
  const next = toml.trim().length > 0 ? `${toml.replace(/\s*$/, "")}\n\n${block}` : block;
  try {
    writeFileSync(file, next);
    return "configured";
  } catch {
    return "skipped";
  }
}

/** Claude Code ships a CLI; add a user-scoped server (no-op if already there). */
export function wireClaudeCode(): WireStatus {
  try {
    execFileSync("claude", ["--version"], { stdio: "ignore" });
  } catch {
    return "absent"; // CLI not on PATH
  }
  try {
    execFileSync("claude", ["mcp", "add", "modelstat", "-s", "user", "--", NPX_COMMAND, ...NPX_ARGS], {
      stdio: "ignore",
    });
    return "configured";
  } catch {
    // Dominant failure is "already exists in this scope".
    return "already";
  }
}

export interface RunWireOptions {
  home?: string;
  platform?: Plat;
  /** shell out to the `claude` CLI (set false in tests) */
  includeClaudeCode?: boolean;
  /** sink for the human-readable report (default: stdout) */
  log?: (line: string) => void;
}

const MARK: Record<WireStatus, string> = { configured: "+", already: "·", absent: "-", skipped: "!" };
const LABEL: Record<WireStatus, string> = {
  configured: "configured",
  already: "already configured",
  absent: "not detected, skipped",
  skipped: "could not write, skipped",
};

/** Wire every detected tool. Best-effort — always resolves to exit code 0. */
export async function runWire(opts: RunWireOptions = {}): Promise<number> {
  const home = opts.home ?? homedir();
  const plat = opts.platform ?? osPlatform();
  const includeClaudeCode = opts.includeClaudeCode ?? true;
  const out = opts.log ?? ((l: string) => process.stdout.write(`${l}\n`));

  const results: WireResult[] = [];
  if (includeClaudeCode) results.push({ name: "Claude Code", status: wireClaudeCode() });
  for (const t of jsonTargets(home, plat)) results.push({ name: t.name, status: wireJsonTarget(t) });
  results.push({ name: "Codex", status: wireCodex(home) });

  out("modelstat MCP — wiring your AI tools:");
  for (const r of results) out(`  ${MARK[r.status]} ${r.name} — ${LABEL[r.status]}`);
  const n = results.filter((r) => r.status === "configured").length;
  out(
    n > 0
      ? `Configured ${n} tool${n === 1 ? "" : "s"}. Restart any open tool to load the modelstat MCP.`
      : "Nothing new to configure (already set up, or no supported tools detected).",
  );
  return 0;
}
