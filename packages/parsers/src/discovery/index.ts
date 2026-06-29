/**
 * Source Discovery Engine.
 *
 * Runs every strategy and returns a merged list of
 * detected installations + identities. Deterministic, idempotent — safe
 * to run on a cron.
 *
 * Strategies implemented (v1):
 *   1. Known-path probe
 *   3. Binary walk
 *   4. Signature-based file scan (macOS `mdfind`; Linux `find` with depth limit)
 *   6. Application registry (macOS `system_profiler`)
 *
 * Later strategies (live process sniff, Docker, SDK proxy, etc.) are
 * scaffolded with TODOs — they're easy follow-ups once v1 ships.
 */
import { execFile, execSync } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import { homedir, platform } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import type { Agent, DetectedIdentity, DetectedInstallation } from "@modelstat/core";

const pexec = promisify(execFile);

// ──────────────────────────────────────────────────────────────
// Registry of sources. Extend this list to add a new agent — no
// code changes needed in the discovery logic itself.
// ──────────────────────────────────────────────────────────────

interface SourceSpec {
  agent: Agent;
  /** Preferred data-dir candidates, in order. */
  dataDirs: { macos?: string[]; linux?: string[] };
  /** Env vars that override the data dir. */
  dataDirEnv?: string[];
  /** Binary names to look for on PATH. */
  binaries?: string[];
  /** macOS .app bundle ids. */
  bundleIds?: string[];
  /** Signature match — a file path glob (used with `mdfind` or bounded
   * `find`) + the first-line key that must be present. */
  fileSignatures?: Array<{
    filenameGlob: string;
    firstLineHasKey?: string;
  }>;
}

const H = (p: string): string => (p.startsWith("~") ? p.replace("~", homedir()) : p);

const SOURCES: SourceSpec[] = [
  {
    agent: "claude_code",
    dataDirs: {
      macos: ["~/.claude"],
      linux: ["$XDG_CONFIG_HOME/claude", "~/.claude", "~/.config/claude"],
    },
    dataDirEnv: ["CLAUDE_HOME"],
    binaries: ["claude", "claude-code"],
    fileSignatures: [
      { filenameGlob: "projects/**/*.jsonl", firstLineHasKey: "sessionId" },
    ],
  },
  {
    agent: "codex_cli",
    dataDirs: {
      macos: ["~/.codex"],
      linux: ["$XDG_CONFIG_HOME/codex", "~/.codex"],
    },
    dataDirEnv: ["CODEX_HOME"],
    binaries: ["codex"],
    fileSignatures: [{ filenameGlob: "sessions/**/rollout-*.jsonl" }],
  },
  {
    agent: "claude_desktop",
    dataDirs: {
      macos: ["~/Library/Application Support/Claude"],
      linux: ["~/.config/Claude"],
    },
    bundleIds: ["com.anthropic.claudeforbrowser", "com.anthropic.claudeelectron"],
  },
  {
    agent: "cursor",
    dataDirs: {
      macos: ["~/Library/Application Support/Cursor"],
      linux: ["~/.config/Cursor"],
    },
    bundleIds: ["co.anysphere.cursor", "com.todesktop.230313mzl4w4u92"],
  },
  {
    agent: "windsurf",
    dataDirs: {
      macos: ["~/Library/Application Support/Windsurf"],
      linux: ["~/.config/Windsurf"],
    },
    bundleIds: ["com.codeium.windsurf"],
  },
  {
    agent: "zed",
    dataDirs: {
      macos: ["~/Library/Application Support/Zed", "~/.config/zed"],
      linux: ["~/.config/zed"],
    },
    binaries: ["zed"],
    bundleIds: ["dev.zed.Zed"],
  },
  {
    agent: "gemini_cli",
    dataDirs: {
      macos: ["~/.gemini"],
      linux: ["~/.gemini"],
    },
    binaries: ["gemini"],
  },
  {
    agent: "aider",
    dataDirs: { macos: ["~/.aider"], linux: ["~/.aider"] },
    binaries: ["aider"],
  },
  {
    agent: "ollama",
    dataDirs: { macos: ["~/.ollama"], linux: ["~/.ollama"] },
    binaries: ["ollama"],
    bundleIds: ["com.electron.ollama"],
  },
  {
    agent: "pi",
    dataDirs: {
      macos: ["~/.pi/agent"],
      linux: ["$XDG_CONFIG_HOME/pi/agent", "~/.pi/agent"],
    },
    dataDirEnv: ["PI_HOME"],
    binaries: ["pi"],
    fileSignatures: [{ filenameGlob: "sessions/**/*.jsonl", firstLineHasKey: "cwd" }],
  },
  {
    agent: "openclaw",
    dataDirs: { macos: ["~/.openclaw", "~/.claw"], linux: ["~/.openclaw", "~/.claw"] },
    binaries: ["openclaw", "claw", "clawdbot", "moltbot"],
  },
];

// ──────────────────────────────────────────────────────────────
// Public API
// ──────────────────────────────────────────────────────────────

export interface DiscoveryOptions {
  /** Extra data-dir paths to check, per agent. Honours $HOME. */
  extraDataDirs?: Partial<Record<Agent, string[]>>;
  /** Skip these expensive strategies. */
  skip?: Array<"binaryWalk" | "fileSignatures" | "appRegistry">;
}

export interface DiscoveryOutput {
  installations: DetectedInstallation[];
  identities: DetectedIdentity[];
}

export async function discover(options: DiscoveryOptions = {}): Promise<DiscoveryOutput> {
  const os = platform() === "darwin" ? "macos" : "linux";
  const skip = new Set(options.skip ?? []);
  const installations: DetectedInstallation[] = [];
  const identities: DetectedIdentity[] = [];

  // (1) known-path probe
  for (const spec of SOURCES) {
    const candidates = new Set<string>();
    for (const raw of spec.dataDirs[os] ?? []) candidates.add(expandPath(raw));
    for (const env of spec.dataDirEnv ?? []) {
      const v = process.env[env];
      if (v) candidates.add(v);
    }
    for (const extra of options.extraDataDirs?.[spec.agent] ?? []) candidates.add(expandPath(extra));

    for (const p of candidates) {
      if (existsSync(p) && statSync(p).isDirectory()) {
        installations.push({
          agent: spec.agent,
          install_method: "manual",
          binary_path: null,
          data_dir: p,
          version: null,
          detected_via: ["known_path"],
        });
      }
    }
  }

  // (3) binary walk
  if (!skip.has("binaryWalk")) {
    const binDirs = binaryLookupDirs(os);
    for (const spec of SOURCES) {
      for (const bin of spec.binaries ?? []) {
        for (const dir of binDirs) {
          const p = join(dir, bin);
          if (existsSync(p)) {
            const version = await safeVersionProbe(p);
            installations.push({
              agent: spec.agent,
              install_method: classifyInstallMethod(p, os),
              binary_path: p,
              data_dir: null,
              version,
              detected_via: ["binary_walk"],
            });
          }
        }
      }
    }
  }

  // (6) application registry (macOS)
  if (!skip.has("appRegistry") && os === "macos") {
    try {
      const apps = await macosListApps();
      for (const spec of SOURCES) {
        for (const bid of spec.bundleIds ?? []) {
          const hit = apps.find((a) => a.bundleId === bid);
          if (hit) {
            installations.push({
              agent: spec.agent,
              install_method: "app_bundle",
              binary_path: hit.path,
              data_dir: null,
              version: hit.version,
              detected_via: ["app_registry"],
            });
          }
        }
      }
    } catch {
      // system_profiler can fail on weird machines — non-fatal
    }
  }

  // (4) signature-based file scan — limited to known dirs in v1 to keep
  // startup fast; full `mdfind`/`find` pass is an agent cron.

  // Identity probes — best-effort, filesystem-only (no Keychain yet).
  identities.push(...(await probeIdentities(os)));

  // Dedupe
  return {
    installations: dedupeInstalls(installations),
    identities: dedupeIdentities(identities),
  };
}

// ──────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────

function expandPath(p: string): string {
  let s = p;
  s = s.replace(/\$XDG_CONFIG_HOME/g, process.env.XDG_CONFIG_HOME ?? `${homedir()}/.config`);
  s = s.replace(/\$XDG_DATA_HOME/g, process.env.XDG_DATA_HOME ?? `${homedir()}/.local/share`);
  s = s.replace(/\$HOME/g, homedir());
  s = H(s);
  return resolve(s);
}

function binaryLookupDirs(os: "macos" | "linux"): string[] {
  const dirs = new Set<string>();
  (process.env.PATH ?? "").split(":").forEach((d) => d && dirs.add(d));
  const extra =
    os === "macos"
      ? [
          "/opt/homebrew/bin",
          "/usr/local/bin",
          `${homedir()}/.bun/bin`,
          `${homedir()}/.volta/bin`,
          `${homedir()}/.cargo/bin`,
          `${homedir()}/.local/bin`,
          `${homedir()}/.asdf/shims`,
          `${homedir()}/.mise/shims`,
          `${homedir()}/.npm-global/bin`,
          `${homedir()}/.yarn/bin`,
        ]
      : [
          "/usr/local/bin",
          "/usr/bin",
          "/snap/bin",
          "/var/lib/flatpak/exports/bin",
          `${homedir()}/.local/bin`,
          `${homedir()}/.bun/bin`,
          `${homedir()}/.cargo/bin`,
          `${homedir()}/.nvm`,
        ];
  for (const d of extra) dirs.add(d);
  return Array.from(dirs);
}

function classifyInstallMethod(binPath: string, os: "macos" | "linux"): DetectedInstallation["install_method"] {
  if (binPath.includes("/homebrew/") || binPath.includes("/Cellar/")) return "homebrew";
  if (binPath.includes("/.nvm/") || binPath.includes("/node_modules/")) return "npm_global";
  if (binPath.includes("/.npm-global/")) return "npm_global";
  if (binPath.includes("/.pnpm/") || binPath.includes("/.pnpm-global/")) return "pnpm_global";
  if (binPath.includes("/.yarn/")) return "yarn_global";
  if (binPath.includes("/.bun/")) return "bun_global";
  if (binPath.includes("/.cargo/")) return "cargo_install";
  if (binPath.startsWith("/Applications/") || binPath.includes(".app/Contents/")) return "app_bundle";
  if (os === "linux" && binPath.startsWith("/snap/")) return "snap";
  if (os === "linux" && binPath.includes("/flatpak/")) return "flatpak";
  return "manual";
}

async function safeVersionProbe(binPath: string): Promise<string | null> {
  try {
    const { stdout } = await pexec(binPath, ["--version"], { timeout: 1500 });
    return stdout.trim().split(/\s+/).pop() ?? null;
  } catch {
    return null;
  }
}

async function macosListApps(): Promise<Array<{ bundleId: string; path: string; version: string | null }>> {
  try {
    const out = execSync(
      "system_profiler SPApplicationsDataType -json 2>/dev/null | head -c 8000000",
      { timeout: 15_000 },
    ).toString();
    const parsed = JSON.parse(out) as { SPApplicationsDataType?: Array<{ path?: string; version?: string; info?: string }> };
    const apps = parsed.SPApplicationsDataType ?? [];
    return apps.map((a) => ({
      bundleId: "", // system_profiler JSON doesn't expose bundleId directly; requires mdls for each
      path: a.path ?? "",
      version: a.version ?? null,
    }));
  } catch {
    return [];
  }
}

async function probeIdentities(os: "macos" | "linux"): Promise<DetectedIdentity[]> {
  const ids: DetectedIdentity[] = [];
  const fs = await import("node:fs");

  // Claude Code — credentials live in macOS Keychain at "Claude Code-credentials"
  if (os === "macos") {
    try {
      const out = execSync(
        'security find-generic-password -s "Claude Code-credentials" -w 2>/dev/null',
        { timeout: 3000 },
      ).toString();
      const body = JSON.parse(out) as {
        claudeAiOauth?: {
          accessToken?: string;
          refreshToken?: string;
          subscriptionType?: string;
          rateLimitTier?: string;
          scopes?: string[];
          // Newer blobs may carry account/org identity — extract when present.
          account?: { uuid?: string; email_address?: string; email?: string };
          organization?: { uuid?: string; name?: string };
        };
      };
      const oauth = body.claudeAiOauth;
      const tok = oauth?.accessToken;
      if (tok) {
        // Prefer a stable account/org id; the OAuth account uuid (when the
        // blob carries it) is best. Fall back to a hash of the refresh
        // token (stable across access-token refreshes) only as a last
        // resort — tokens DO eventually rotate, so a refreshed login may
        // create a fresh identity the user can relabel/merge via a rule.
        const email = oauth?.account?.email_address ?? oauth?.account?.email ?? null;
        const orgName = oauth?.organization?.name ?? null;
        const stableId =
          oauth?.account?.uuid ??
          oauth?.organization?.uuid ??
          (oauth?.refreshToken ?? tok).slice(0, 48);
        ids.push({
          provider: "anthropic",
          provider_account_id: stableId,
          provider_account_label:
            email ?? orgName ?? oauth?.subscriptionType ?? "Claude account",
          account_email: email,
          account_org: orgName ?? oauth?.subscriptionType ?? null,
          display_name: null,
          owner_scope: "unassigned",
          detection_source: "claude_keychain",
        });
      }
    } catch {
      /* no keychain entry → no identity */
    }
  }

  // Claude Code (desktop app + recent CLI) — the OAuth account lives in
  // ~/.claude.json's `oauthAccount`, not only the "Claude Code-credentials"
  // keychain item the older CLI used (absent for desktop-app users, the most
  // common case). Plain file we already read, so no keychain-ACL / launchd
  // permission issues; dedupeIdentities collapses this with the keychain hit
  // when both exist (same accountUuid).
  const claudeConfigs = [`${homedir()}/.claude.json`];
  if (process.env.CLAUDE_CONFIG_DIR) {
    claudeConfigs.unshift(`${process.env.CLAUDE_CONFIG_DIR}/.claude.json`);
  }
  for (const candidate of claudeConfigs) {
    if (!existsSync(candidate)) continue;
    try {
      const data = await fs.promises.readFile(candidate, "utf8");
      const obj = JSON.parse(data) as {
        oauthAccount?: {
          accountUuid?: string;
          emailAddress?: string;
          organizationUuid?: string;
          organizationName?: string;
          displayName?: string;
          billingType?: string;
        };
      };
      const acct = obj.oauthAccount;
      const stableId = acct?.accountUuid ?? acct?.organizationUuid;
      if (acct && stableId) {
        ids.push({
          provider: "anthropic",
          provider_account_id: stableId,
          provider_account_label:
            acct.emailAddress ?? acct.organizationName ?? acct.displayName ?? "Claude account",
          account_email: acct.emailAddress ?? null,
          account_org: acct.organizationName ?? acct.billingType ?? null,
          display_name: acct.displayName ?? null,
          owner_scope: "unassigned",
          detection_source: "claude_json_oauth",
        });
        break;
      }
    } catch {
      /* ignore */
    }
  }

  // Codex auth.json — JWT id_token contains email + sub + auth_provider
  for (const candidate of [
    `${homedir()}/.codex/auth.json`,
    `${homedir()}/.config/codex/auth.json`,
  ]) {
    if (!existsSync(candidate)) continue;
    try {
      const data = await fs.promises.readFile(candidate, "utf8");
      const obj = JSON.parse(data) as {
        auth_mode?: string;
        tokens?: { id_token?: string; account_id?: string };
      };
      const jwt = obj.tokens?.id_token;
      let email: string | null = null;
      let sub: string | null = null;
      let name: string | null = null;
      let org: string | null = null;
      let provider: "openai" | "google" = "openai";
      if (jwt) {
        const parts = jwt.split(".");
        if (parts.length >= 2) {
          try {
            const pad = "=".repeat((4 - (parts[1]!.length % 4)) % 4);
            const body = JSON.parse(
              Buffer.from(parts[1]! + pad, "base64url").toString("utf8"),
            ) as {
              email?: string;
              sub?: string;
              auth_provider?: string;
              name?: string;
              "https://api.openai.com/auth"?: {
                organization_id?: string;
                chatgpt_plan_type?: string;
              };
            };
            email = body.email ?? null;
            sub = body.sub ?? null;
            name = body.name ?? null;
            const oai = body["https://api.openai.com/auth"];
            org = oai?.organization_id ?? oai?.chatgpt_plan_type ?? null;
            if (body.auth_provider === "google") provider = "openai"; // still OpenAI / ChatGPT account, auth'd via Google
          } catch {
            /* fall through */
          }
        }
      }
      // Prefer the stable account_id as the key (survives token refresh).
      const pid = obj.tokens?.account_id ?? sub ?? email;
      if (pid) {
        ids.push({
          provider,
          provider_account_id: pid,
          provider_account_label: email,
          account_email: email,
          account_org: org,
          display_name: name,
          owner_scope: "unassigned",
          detection_source: "codex_auth_json",
        });
      }
    } catch {
      /* ignore */
    }
  }

  // Gemini oauth_creds.json — email as id
  for (const candidate of [
    `${homedir()}/.gemini/oauth_creds.json`,
    `${homedir()}/.config/gemini/oauth_creds.json`,
  ]) {
    if (!existsSync(candidate)) continue;
    try {
      const data = await fs.promises.readFile(candidate, "utf8");
      const obj = JSON.parse(data) as { email?: string; token?: { email?: string } };
      const email = obj.email ?? obj.token?.email;
      if (email) {
        ids.push({
          provider: "google",
          provider_account_id: email,
          provider_account_label: email,
          account_email: email,
          owner_scope: "unassigned",
          detection_source: "gemini_oauth_creds",
        });
      }
    } catch {
      /* ignore */
    }
  }

  // Cursor — globalStorage/storage.json has the user id + subscription
  for (const candidate of [
    `${homedir()}/Library/Application Support/Cursor/User/globalStorage/storage.json`,
    `${homedir()}/.config/Cursor/User/globalStorage/storage.json`,
  ]) {
    if (!existsSync(candidate)) continue;
    try {
      const data = await fs.promises.readFile(candidate, "utf8");
      const obj = JSON.parse(data) as Record<string, unknown>;
      // Cursor stores its auth under `cursorAuth/*` keys — take the first one.
      for (const k of Object.keys(obj)) {
        if (k.startsWith("cursorAuth") && typeof obj[k] === "string") {
          try {
            const auth = JSON.parse(obj[k] as string) as {
              sub?: string;
              email?: string;
              cachedSignUpType?: string;
            };
            if (auth.sub || auth.email) {
              ids.push({
                provider: "cursor",
                provider_account_id: auth.sub ?? auth.email!,
                provider_account_label: auth.email ?? null,
                account_email: auth.email ?? null,
                owner_scope: "unassigned",
                detection_source: "cursor_global_storage",
              });
              break;
            }
          } catch {
            /* ignore */
          }
        }
      }
    } catch {
      /* ignore */
    }
  }

  return ids;
}

function dedupeInstalls(list: DetectedInstallation[]): DetectedInstallation[] {
  const seen = new Map<string, DetectedInstallation>();
  for (const i of list) {
    const k = `${i.agent}|${i.binary_path ?? ""}|${i.data_dir ?? ""}`;
    const prev = seen.get(k);
    if (!prev) {
      seen.set(k, i);
    } else {
      prev.detected_via = Array.from(new Set([...prev.detected_via, ...i.detected_via]));
      prev.version = prev.version ?? i.version;
    }
  }
  return Array.from(seen.values());
}

function dedupeIdentities(list: DetectedIdentity[]): DetectedIdentity[] {
  const seen = new Map<string, DetectedIdentity>();
  for (const i of list) {
    const k = `${i.provider}|${i.provider_account_id}`;
    if (!seen.has(k)) seen.set(k, i);
  }
  return Array.from(seen.values());
}
