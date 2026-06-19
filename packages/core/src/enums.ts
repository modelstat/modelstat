/**
 * Canonical enumerations shared across every consumer of the modelstat
 * ingest contract. Keep this list small and meaningful — adding a value
 * is cheap; changing one is a migration.
 */

export const AGENTS = [
  "claude_code",
  "claude_desktop",
  "codex_cli",
  "codex_desktop",
  "cursor",
  "windsurf",
  "void",
  "zed",
  "vscode_copilot",
  "vscode_copilot_chat",
  "vscode_cline",
  "vscode_continue",
  "vscode_codeium",
  "jetbrains_copilot",
  "jetbrains_ai",
  "xcode_copilot",
  "gemini_cli",
  "aider",
  "opencode",
  "crush",
  "kimi",
  "openclaw",
  "hermes",
  "ollama",
  "raw_sdk_anthropic",
  "raw_sdk_openai",
  "raw_sdk_google",
  // Web chat UIs (Chrome-extension daemon). Categorically distinct
  // from *_cli / *_desktop agents — same provider, different surface.
  "chatgpt_web",
  "claude_web",
  "gemini_web",
  "grok_web",
  "unknown",
] as const;
export type Agent = (typeof AGENTS)[number];

export const PROVIDERS = [
  "anthropic",
  "openai",
  "google",
  "cursor",
  "github", // copilot
  "deepseek",
  "moonshot",
  "mistral",
  "xai",
  "ollama_local",
  "unknown",
] as const;
export type Provider = (typeof PROVIDERS)[number];

/** Top-level categorisation of a session. Closed enum — keeps the cross-org
 * taxonomy sane. Finer-grained custom labels live under `areas`. */
export const WORK_TYPES = [
  "planning",
  "implementation",
  "debugging",
  "review",
  "refactoring",
  "testing",
  "docs",
  "devops",
  "research",
  "ops",
  "chat",
  "misc",
] as const;
export type WorkType = (typeof WORK_TYPES)[number];

export const IDENTITY_OWNER_SCOPES = ["org", "personal", "unassigned"] as const;
export type IdentityOwnerScope = (typeof IDENTITY_OWNER_SCOPES)[number];

export const INSTALL_METHODS = [
  "homebrew",
  "homebrew_cask",
  "npm_global",
  "pnpm_global",
  "yarn_global",
  "bun_global",
  "cargo_install",
  "pipx",
  "app_bundle", // macOS .app
  "deb",
  "rpm",
  "aur",
  "nix",
  "flatpak",
  "snap",
  "manual",
  "docker",
  "chrome_extension",
  "unknown",
] as const;
export type InstallMethod = (typeof INSTALL_METHODS)[number];

export const OS_FAMILIES = ["macos", "linux", "windows", "other"] as const;
export type OSFamily = (typeof OS_FAMILIES)[number];

/** Membership roles inside an org. */
export const MEMBERSHIP_ROLES = [
  "owner",
  "admin",
  "finance",
  "member",
  "viewer",
] as const;
export type MembershipRole = (typeof MEMBERSHIP_ROLES)[number];

export const EVENT_KINDS = [
  "user_message",
  "assistant_message",
  "tool_call",
  "tool_result",
  "summary",
] as const;
export type EventKind = (typeof EVENT_KINDS)[number];

/** Outcome of one tool invocation (see ToolCallWire in schemas.ts).
 * `unknown` = the tool_use never got a matching tool_result in the
 * transcript (still running, crashed, or the file was cut off). */
export const TOOL_CALL_STATUSES = [
  "success",
  "error",
  "denied",
  "timeout",
  "unknown",
] as const;
export type ToolCallStatus = (typeof TOOL_CALL_STATUSES)[number];

/**
 * Default taxonomy roots seeded for a new org. This is NOT a closed
 * enum — roots can be added/removed/renamed. Keep these generic enough
 * that most orgs keep most of them, narrow enough to be immediately
 * useful out of the box.
 *
 * Each entry: slug used as root_key + display `name` + `color` hint
 * the dashboard starts with.
 */
export const DEFAULT_TAXONOMY_ROOTS: ReadonlyArray<{
  slug: string;
  name: string;
  color: string;
  description: string;
}> = [
  { slug: "projects",     name: "Projects",     color: "#60a5fa", description: "Which codebase or product are we working in?" },
  { slug: "domains",      name: "Domains",      color: "#f472b6", description: "Which functional area (auth, billing, ingestion, …)?" },
  { slug: "initiatives",  name: "Initiatives",  color: "#fb923c", description: "Multi-week efforts the work rolls up into." },
  { slug: "environments", name: "Environments", color: "#a3e635", description: "Prod vs staging vs dev." },
  { slug: "persons",      name: "Persons",      color: "#c084fc", description: "Teammates referenced in the work." },
  { slug: "providers",    name: "Providers",    color: "#22d3ee", description: "Anthropic, OpenAI, Google, …" },
  { slug: "models",       name: "Models",       color: "#34d399", description: "Specific model versions used." },
  { slug: "work_types",   name: "Work types",   color: "#facc15", description: "Refactor, feature, bugfix, review, …" },
  { slug: "agents",       name: "Agents",       color: "#f87171", description: "Claude Code, Cursor, ChatGPT Web, …" },
  { slug: "components",   name: "Components",   color: "#818cf8", description: "File-or-module-level slices touched by the session." },
  // NB: "agents" above = the AI client (claude_code, cursor, …). "tool_calls"
  // = the tools those agents invoke — builtin (Bash, Read) and MCP (mcp:github/create_pr).
  { slug: "tool_calls",   name: "Tool calls",   color: "#2dd4bf", description: "Tools invoked by agents — builtin (Bash, Read, …) and MCP (mcp:server/tool)." },
];

/** Daemon heartbeat phases. Both CLI and extension emit these via the
 * same HeartbeatPayload schema (packages/core/src/schemas.ts → HeartbeatPayload).
 * Extension treats unused phases as "never reached" — still in the enum
 * so the contract stays uniform. */
export const DAEMON_PHASES = [
  "starting",
  "discovering",
  "idle",
  "scanning",
  "processing",
  "uploading",
  "watching",
  "offline",
  "error",
] as const;
export type DaemonPhase = (typeof DAEMON_PHASES)[number];

/** How confidently the classifier assigned a work_type / area. */
export const CLASSIFICATION_CONFIDENCE = [
  "hard", // deterministic (from git remote, filenames, etc.)
  "high", // >0.8 cosine
  "medium", // 0.6–0.8
  "low", // <0.6
  "unclassified",
] as const;
export type ClassificationConfidence = (typeof CLASSIFICATION_CONFIDENCE)[number];
