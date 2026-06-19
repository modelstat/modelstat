/**
 * Watch mode: keeps running, re-scans on filesystem changes.
 *
 * Uses chokidar to watch every known AI-tool data directory. On change
 * (new JSONL or append), runs `scanAll()` with cursors, so only delta
 * events are uploaded.
 */
import chokidar from "chokidar";
import { existsSync } from "node:fs";
import { homedir, platform } from "node:os";
import { join } from "node:path";
import { scanAll } from "./scan.js";

/** Cross-platform watch targets. Non-existent paths are silently
 * ignored by chokidar, but we also pre-filter so the startup log reads
 * nicely on a fresh machine. */
function resolveWatchDirs(): string[] {
  const home = homedir();
  const xdgConfig = process.env.XDG_CONFIG_HOME ?? join(home, ".config");
  const xdgData = process.env.XDG_DATA_HOME ?? join(home, ".local/share");

  const candidates = [
    // universal (default HOME-rooted CLI data dirs)
    join(home, ".claude/projects"),
    join(home, ".codex/sessions"),
    join(home, ".cursor/ai-tracking"),
    join(home, ".gemini"),
    join(home, ".aider"),

    // XDG / Linux
    join(xdgConfig, "claude/projects"),
    join(xdgConfig, "codex/sessions"),
    join(xdgConfig, "Cursor/User/workspaceStorage"),
    join(xdgConfig, "Code/User/workspaceStorage"),
    join(xdgConfig, "Code - Insiders/User/workspaceStorage"),
    join(xdgData, "claude/projects"),

    // macOS
    ...(platform() === "darwin"
      ? [
          join(home, "Library/Application Support/Cursor/User/workspaceStorage"),
          join(home, "Library/Application Support/Claude"),
          join(home, "Library/Application Support/Code/User/workspaceStorage"),
          join(home, "Library/Application Support/Windsurf/User/workspaceStorage"),
          join(home, "Library/Application Support/Zed"),
        ]
      : []),
  ];
  return Array.from(new Set(candidates)).filter((p) => existsSync(p));
}

const DIRS = resolveWatchDirs();

let scanning = false;
let pending = false;
let lastScan = 0;

async function safeScan(reason: string): Promise<void> {
  if (scanning) {
    pending = true;
    return;
  }
  scanning = true;
  try {
    const now = Date.now();
    if (now - lastScan < 3_000) return; // debounce
    lastScan = now;
    console.log(`[${new Date().toISOString()}] scan (${reason})`);
    const r = await scanAll();
    if (r.batchesUploaded || r.eventsUploaded) {
      console.log(
        `  → ${r.segmentsUploaded} segments · ${r.eventsUploaded} events in ${r.batchesUploaded} batches`,
      );
    }
  } catch (e) {
    console.warn("  ! scan failed:", (e as Error).message);
  } finally {
    scanning = false;
    if (pending) {
      pending = false;
      setTimeout(() => safeScan("pending"), 500);
    }
  }
}

export async function watchForever(): Promise<void> {
  // Initial full scan
  await safeScan("startup");

  const watcher = chokidar.watch(DIRS, {
    persistent: true,
    ignoreInitial: true,
    depth: 10,
    awaitWriteFinish: { stabilityThreshold: 500, pollInterval: 200 },
  });

  watcher
    .on("add", (p) => {
      if (p.endsWith(".jsonl")) void safeScan(`add ${p}`);
    })
    .on("change", (p) => {
      if (p.endsWith(".jsonl")) void safeScan(`change ${p}`);
    })
    .on("error", (e) => console.warn("watcher error:", e));

  // Also run every 5 minutes as a backstop.
  setInterval(() => void safeScan("interval"), 5 * 60 * 1000);

  console.log(`watching: ${DIRS.join(", ")}`);
  // Keep alive forever — the service manager handles restart.
  await new Promise<void>(() => {});
}
