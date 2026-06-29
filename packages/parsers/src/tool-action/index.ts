/**
 * On-device, deterministic structural extraction for one tool call.
 *
 * Produces ONLY the cheap, generic STRUCTURAL facts + the compliance-redacted
 * command — no model, no vocabulary, browser-safe and synchronous. SPEC 0004:
 * the wire carries no semantic fields at all; the backend derives the WHOLE
 * operation frame (action / object / system / environment / effect /
 * multiplicity / label / blast_radius) from `command_redacted` with a better,
 * re-runnable model, cached per distinct command (so stats stay accurate and
 * future-proof, and re-derive when the model or steering prompt improves).
 *
 * PRIVACY: the only command-derived outputs are `param_shape` (every value
 * masked to `§`) and `command_redacted` (secrets / PII stripped by `redact()`,
 * in-repo paths relativised against `cwd`). The raw command is never returned.
 */
import { paramShape, redact, type ToolAction } from "@modelstat/core";

import { extractExecutable } from "./executable.js";

export * from "./executable.js";
export * from "./scripts.js";

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

/**
 * Malicious-size guard, mirrored from the backend
 * (`MAX_TOOL_ACTION_PARAM_SHAPE_CHARS` / `MAX_TOOL_ACTION_COMMAND_CHARS`). The
 * full value-masked / redacted command rides the wire untouched below this;
 * over it we clamp (Unicode-scalar safe) rather than drop. Long random blobs
 * (keys / base64 / hashes) are collapsed by {@link redact}, not by this cap.
 */
const MAX_FIELD_CHARS = 16_384;

/** Truncate to at most `max` Unicode code points (matches the backend's
 * char-boundary clamp; never splits a surrogate pair). */
function clampChars(s: string, max: number): string {
  const cps = [...s];
  return cps.length > max ? cps.slice(0, max).join("") : s;
}

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
    // executable: the leading *meaningful* program (peels cd / wrappers / env
    // assignments / pipelines), never a raw fragment or secret. See
    // `extractExecutable`. param_shape keeps its own contract: the value-masked
    // skeleton of everything after the first whitespace token.
    executable = extractExecutable(command);
    const args = command.trim().split(/\s+/).slice(1).join(" ");
    param_shape = clampChars(paramShape(args), MAX_FIELD_CHARS) || null;
    command_redacted =
      clampChars(redact(command, call.cwd ?? undefined).text, MAX_FIELD_CHARS) || null;
  }

  return {
    surface,
    executable,
    param_shape,
    command_redacted,
    scripts: [],
    // Per-surface provenance. shell bumped to v3 (normalized executable, see
    // `extractExecutable`); builtin/mcp extraction is unchanged → still v1.
    extractor: `${surface}.${surface === "shell" ? "v3" : "v1"}`,
  };
}

/**
 * The local-only context the Node agent needs to read + summarise a shell
 * call's referenced script files: the RAW command + cwd. Returns null for
 * non-shell calls (mcp/builtin) — nothing to read.
 *
 * This is the ONLY function that surfaces the raw command, and it travels on
 * `ParseResult.scriptContexts` (local-only), NEVER on the wire — the shipped
 * `ToolAction` keeps only `command_redacted`. See {@link extractToolAction}.
 */
export function extractLocalToolContext(call: ToolActionInput): {
  command: string;
  cwd: string | null;
} | null {
  if (call.server.startsWith("mcp:")) return null;
  const command = shellCommandOf(call.input);
  if (command == null) return null;
  return { command, cwd: call.cwd ?? null };
}

/** The shell command string inside a tool input, or null when this isn't a
 * shell-style call. Generic: a string input, or a string/argv `command`/`cmd`
 * field — no tool-name allowlist. */
function shellCommandOf(input: unknown): string | null {
  if (typeof input === "string") return input.trim() ? input : null;
  if (input && typeof input === "object") {
    const cmd =
      (input as Record<string, unknown>).command ?? (input as Record<string, unknown>).cmd;
    if (typeof cmd === "string") return cmd.trim() ? cmd : null;
    if (Array.isArray(cmd)) {
      const parts = cmd.filter((p): p is string => typeof p === "string");
      if (parts.length) return parts.join(" ");
    }
  }
  return null;
}
