/**
 * On-device, deterministic structural extraction for one tool call.
 *
 * Produces ONLY the cheap, generic facts + the compliance-redacted command —
 * no model, no vocabulary, browser-safe and synchronous. The semantic fields
 * (`action` / `object` / `keywords` / `abstract` / `qualifiers`) are left
 * null/empty on purpose: the backend derives those from `command_redacted`
 * with a better, re-runnable model (so stats stay accurate and future-proof).
 *
 * PRIVACY: the only command-derived outputs are `param_shape` (every value
 * masked to `§`) and `command_redacted` (secrets / PII stripped by `redact()`,
 * in-repo paths relativised against `cwd`). The raw command is never returned.
 */
import { paramShape, redact, type ToolAction } from "@modelstat/core";

/** What the parser has in hand for one observed call at draft-build time
 * (before the args are hashed away). */
export interface ToolActionInput {
  /** `builtin` or `mcp:<server>`. */
  server: string;
  /** Bare tool name (`Bash`, `create_pr`, `shell`). */
  name: string;
  /** The raw tool input (a shell `{ command }` object, an args object, …). */
  input: unknown;
  /** The event's cwd, if known — lets redaction keep in-repo paths relative. */
  cwd?: string | null;
}

/** Mirror of the backend's per-command cap. */
const MAX_COMMAND_REDACTED = 1000;

/**
 * Extract the deterministic structural facts for one tool call. The returned
 * `ToolAction` is wire-shaped; semantic fields are null/empty (server-derived).
 */
export function extractToolAction(call: ToolActionInput): ToolAction {
  const isMcp = call.server.startsWith("mcp:");
  const command = isMcp ? null : shellCommandOf(call.input);
  const surface = isMcp ? "mcp" : command != null ? "shell" : "builtin";

  let executable: string | null = call.name || null;
  let param_shape: string | null = null;
  let command_redacted: string | null = null;

  if (command != null) {
    const [head = "", ...rest] = command.trim().split(/\s+/);
    executable = basename(head) || null;
    param_shape = paramShape(rest.join(" ")) || null;
    command_redacted =
      redact(command, call.cwd ?? undefined).text.slice(0, MAX_COMMAND_REDACTED) || null;
  }

  return {
    surface,
    executable,
    action: null,
    object: null,
    qualifiers: [],
    param_shape,
    keywords: [],
    abstract: null,
    command_redacted,
    confidence: 0,
    extractor: `${surface}.v1`,
  };
}

/** The shell command string inside a tool input, or null when this isn't a
 * shell-style call. Generic: a string input, or a string/argv `command`/`cmd`
 * field — no tool-name allowlist. */
function shellCommandOf(input: unknown): string | null {
  if (typeof input === "string") return input.trim() ? input : null;
  if (input && typeof input === "object") {
    const cmd = (input as Record<string, unknown>).command ?? (input as Record<string, unknown>).cmd;
    if (typeof cmd === "string") return cmd.trim() ? cmd : null;
    if (Array.isArray(cmd)) {
      const parts = cmd.filter((p): p is string => typeof p === "string");
      if (parts.length) return parts.join(" ");
    }
  }
  return null;
}

/** Basename of a path-or-program token: `/usr/bin/git` → `git`, `./d.sh` → `d.sh`. */
function basename(token: string): string {
  return token.split("/").pop() ?? token;
}
