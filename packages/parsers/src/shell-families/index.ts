/**
 * Shell command → "command families" tokenizer.
 *
 * PRIVACY: the raw command string NEVER leaves the device. This module
 * reduces it to at most {@link MAX_COMMAND_FAMILIES} verbs from the fixed
 * {@link SHELL_FAMILY_ALLOWLIST} below — that reduced list is the only
 * thing that ships (ToolCallWire.command_families). Anything not on the
 * allowlist (custom scripts, file names, free text) is dropped, so the
 * output can never echo user-authored content back to the server.
 *
 * Applied ONLY to execution/shell-classified tools (Bash, shell,
 * local_shell_call, exec_command, run_terminal_cmd). Pure + synchronous.
 */

/** The ~40-verb allowlist. A command family is only ever one of these. */
export const SHELL_FAMILY_ALLOWLIST = [
  "git",
  "npm",
  "pnpm",
  "npx",
  "yarn",
  "node",
  "python",
  "python3",
  "pytest",
  "pip",
  "pip3",
  "cargo",
  "rustc",
  "go",
  "make",
  "cmake",
  "docker",
  "docker-compose",
  "kubectl",
  "helm",
  "terraform",
  "gh",
  "aws",
  "gcloud",
  "az",
  "curl",
  "wget",
  "rg",
  "grep",
  "find",
  "sed",
  "awk",
  "jq",
  "psql",
  "mysql",
  "redis-cli",
  "brew",
  "apt",
  "tsx",
  "vitest",
  "jest",
  "playwright",
  "ruby",
  "bundle",
  "mvn",
  "gradle",
  "ls",
  "cat",
] as const;
export type ShellFamily = (typeof SHELL_FAMILY_ALLOWLIST)[number];

/** Wire cap — ToolCallWire.command_families is `.max(3)`. */
export const MAX_COMMAND_FAMILIES = 3;

const ALLOWLIST: ReadonlySet<string> = new Set(SHELL_FAMILY_ALLOWLIST);

/** Leading wrapper words stripped before reading the verb. (`cd X &&`
 * chains need no special casing: the `&&` split isolates the `cd` part,
 * whose verb then fails the allowlist.) */
const WRAPPERS: ReadonlySet<string> = new Set(["sudo", "env", "time", "nice"]);

/** `VAR=value` prefix assignments, e.g. `RUST_LOG=debug cargo test`. */
const VAR_ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*=/;

/**
 * Reduce a raw shell command to its allowlisted command families.
 *
 * Splits on `&&`, `;`, `|` and newlines (quote-aware: separators inside
 * single/double quotes don't split), strips leading wrappers
 * (sudo/env/time/nice, `VAR=val` assignments), basenames the first token
 * of each part, keeps allowlist members only, dedupes, caps at 3.
 */
export function extractCommandFamilies(command: string): string[] {
  const families: string[] = [];
  for (const part of splitCommandParts(command)) {
    const verb = leadingVerb(part);
    if (!verb || !ALLOWLIST.has(verb)) continue;
    if (!families.includes(verb)) families.push(verb);
    if (families.length >= MAX_COMMAND_FAMILIES) break;
  }
  return families;
}

/** Quote-aware split on `&&`, `;`, `|`, and newlines. "Quote-aware enough
 * for v1": tracks single/double quotes and backslash escapes; does not
 * model subshells/heredocs (a missed split only ever loses a family, it
 * never leaks anything — the allowlist is the final gate). */
function splitCommandParts(command: string): string[] {
  const parts: string[] = [];
  let current = "";
  let inSingle = false;
  let inDouble = false;
  for (let i = 0; i < command.length; i++) {
    const ch = command[i] as string;
    if (!inSingle && ch === "\\") {
      // Escaped char — keep both, never treat the next char as a separator.
      current += ch + (command[i + 1] ?? "");
      i++;
      continue;
    }
    if (ch === "'" && !inDouble) {
      inSingle = !inSingle;
      current += ch;
      continue;
    }
    if (ch === '"' && !inSingle) {
      inDouble = !inDouble;
      current += ch;
      continue;
    }
    if (!inSingle && !inDouble) {
      if (ch === ";" || ch === "|" || ch === "\n") {
        // `||` degenerates into two splits with an empty part between — fine.
        parts.push(current);
        current = "";
        continue;
      }
      if (ch === "&" && command[i + 1] === "&") {
        parts.push(current);
        current = "";
        i++;
        continue;
      }
    }
    current += ch;
  }
  parts.push(current);
  return parts;
}

/** First "real" token of one command part, basenamed. Skips wrapper words,
 * `VAR=val` assignments, and `-`-prefixed wrapper options (`sudo -E`,
 * `env -i`). Option *arguments* (`sudo -u postgres psql`) are not modelled
 * in v1 — they fall through to the allowlist gate and get dropped. */
function leadingVerb(part: string): string | null {
  const tokens = part.trim().split(/\s+/);
  let i = 0;
  while (i < tokens.length) {
    const tok = stripQuotes(tokens[i] ?? "");
    if (tok === "") {
      i++;
      continue;
    }
    if (VAR_ASSIGNMENT.test(tok)) {
      i++;
      continue;
    }
    if (WRAPPERS.has(tok)) {
      i++;
      while (i < tokens.length && (tokens[i] ?? "").startsWith("-")) i++;
      continue;
    }
    const base = tok.split("/").pop() ?? tok;
    return base === "" ? null : base;
  }
  return null;
}

/** `"git" status` → verb `git`. Only strips a matching surrounding pair. */
function stripQuotes(token: string): string {
  if (token.length >= 2) {
    const first = token[0];
    const last = token[token.length - 1];
    if ((first === "'" && last === "'") || (first === '"' && last === '"')) {
      return token.slice(1, -1);
    }
  }
  return token;
}
