/**
 * Deterministic `executable` normalization for a shell command (`shell.v3`).
 *
 * `executable` is the tier-0 dimension that answers "which concrete program did
 * this call run?" — it powers the dashboard's *Executable* breakdown. The v1
 * extractor took the first whitespace token verbatim (`command.split(/\s+/)[0]`),
 * which is wrong for the overwhelmingly common multi-statement shell call: it
 * surfaced `cd` (the leading `cd x && realcmd`), wrapper/keyword heads (`echo`,
 * `eval`, `export`, `set`, `for`), env-var assignment prefixes (`AWS_PROFILE=dev`,
 * `M=…;`), and raw fragments (`core;`, `bin:$PATH"`). On real data ~69% of shell
 * rows were garbage and ~52% were just `cd`. Capturing the assignment prefix also
 * leaked secrets (`CK="sk_live_…"`) into a field that — unlike `command_redacted`
 * — never went through redaction.
 *
 * The fix: find the *leading meaningful program*. We split the command into
 * statements (quote-aware), and for each statement peel the noise that hides the
 * real program, then return its basename.
 *
 * Algorithm (deterministic, privacy-preserving — emits only a program basename
 * or the {@link OTHER_BUCKET} token, never an argument/assignment value):
 *  1. Split on `;`, `&&`, `||`, `|`, `&`, and newlines, honoring single/double
 *     quotes and backslash escapes (so a `;` inside `"…"` does not split).
 *  2. Skip comment statements (`# …`).
 *  3. Within a statement, peel leading tokens that hide the program:
 *       • group/subshell brackets `{ } ( )` and control openers `do then else`;
 *       • shell-function headers (`probe()`);
 *       • exec wrappers (`sudo env time nice nohup xargs command exec …`);
 *       • variable assignments (`FOO=bar`). An assignment whose value is a command
 *         substitution (`WT=$(ssh …)`) yields the inner program (`ssh`).
 *  4. The next token is the candidate: strip a leading `(`/`$(`/backtick, take the
 *     basename, lowercase.
 *       • a shell builtin/keyword that consumes its statement (`cd echo export set
 *         eval for …`) is remembered as a *fallback* and we move to the next
 *         statement (its trailing tokens are that builtin's args, not a program);
 *       • a token that looks like a program (`^[A-Za-z0-9][A-Za-z0-9._+-]*$`, ≤1
 *         dot, not a data-file extension) is returned;
 *       • anything else (a quoted fragment, a flag, a redaction token, a hostname)
 *         ends the statement and we move on.
 *  5. No program in any statement → the remembered builtin (e.g. a bare `cd`),
 *     else {@link OTHER_BUCKET}.
 *
 * The same logic is mirrored on the server in Rust (`modelstat_core::tool_exec`)
 * so already-ingested rows can be backfilled from `command_redacted`.
 */

/** Bucket token for shell calls that don't reduce to a single program. Starts
 * with `(`, which a real program basename never can, so it never collides. */
export const OTHER_BUCKET = "(other)";

/** Max executable length (mirrors the Zod `executable: z.string().max(80)`). A
 * real basename is far shorter; this is a paranoia clamp before we ever return. */
const MAX_EXECUTABLE_CHARS = 80;

/** Exec wrappers / prefixes whose *next* token is the real program. */
const WRAPPERS = new Set([
  "sudo",
  "doas",
  "env",
  "command",
  "exec",
  "builtin",
  "nohup",
  "setsid",
  "time",
  "nice",
  "ionice",
  "chrt",
  "stdbuf",
  "xargs",
  // control-flow openers that immediately precede a command
  "then",
  "do",
  "else",
]);

/** Group/subshell brackets to peel (a statement may open inside one). */
const BRACKETS = new Set(["{", "}", "(", ")"]);

/** Shell builtins / keywords that *consume* their statement: their trailing
 * tokens are their own arguments, not a program. When one of these leads a
 * statement we record it as a fallback and move to the next statement, so
 * `cd x && git push` resolves to `git` while a bare `cd ~` stays `cd`. */
const NOISE_BUILTINS = new Set([
  "cd",
  "pushd",
  "popd",
  "echo",
  "printf",
  "export",
  "unset",
  "set",
  "readonly",
  "typeset",
  "declare",
  "local",
  "alias",
  "source",
  ".",
  "eval",
  ":",
  "true",
  "false",
  "read",
  "wait",
  "trap",
  "umask",
  "shift",
  "return",
  "getopts",
  "hash",
  "let",
  "test",
  "[",
  "[[",
  // loop / conditional keywords
  "for",
  "while",
  "until",
  "if",
  "elif",
  "fi",
  "case",
  "esac",
  "select",
  "function",
  "done",
]);

/** Data-file extensions a program basename never legitimately ends in — these
 * are filename fragments (`run.output`, `notes.md`) that slipped in as heads. */
const DATA_EXTENSIONS = [
  ".output",
  ".txt",
  ".log",
  ".json",
  ".jsonl",
  ".md",
  ".csv",
  ".tmp",
  ".out",
  ".git",
  ".lock",
];

/** Split a command into statements on `;`, `&&`, `||`, `|`, `&`, and newlines,
 * honoring single/double quotes and backslash escapes so separators inside a
 * quoted argument don't split. Brackets are kept inline (peeled per-statement). */
function splitStatements(command: string): string[] {
  const out: string[] = [];
  let cur = "";
  let single = false;
  let double = false;
  for (let i = 0; i < command.length; i++) {
    const c = command[i];
    if (single) {
      cur += c;
      if (c === "'") single = false;
      continue;
    }
    if (double) {
      if (c === "\\" && i + 1 < command.length) {
        cur += c + command[i + 1];
        i++;
        continue;
      }
      cur += c;
      if (c === '"') double = false;
      continue;
    }
    if (c === "'") {
      single = true;
      cur += c;
      continue;
    }
    if (c === '"') {
      double = true;
      cur += c;
      continue;
    }
    if (c === "\\" && i + 1 < command.length) {
      // escaped char (incl. line continuation) — literal, never a separator
      cur += c + command[i + 1];
      i++;
      continue;
    }
    if (c === "#" && (cur === "" || /\s$/.test(cur))) {
      // comment at a word boundary → runs to end of line (drop it, incl. any
      // `;`/`|` inside, so a comment never yields a phantom statement)
      out.push(cur);
      cur = "";
      while (i + 1 < command.length && command[i + 1] !== "\n") i++;
      continue;
    }
    if (c === "\n" || c === ";") {
      out.push(cur);
      cur = "";
      continue;
    }
    if (c === "&" || c === "|") {
      out.push(cur);
      cur = "";
      if (command[i + 1] === c) i++; // collapse && / ||
      continue;
    }
    cur += c;
  }
  out.push(cur);
  return out;
}

/** Basename of a path-or-program token: `/usr/bin/git` → `git`, `./d.sh` → `d.sh`. */
function basename(token: string): string {
  const parts = token.split("/");
  return parts[parts.length - 1] ?? token;
}

/** Strip a leading subshell/substitution opener from a candidate token:
 * `(rg` → `rg`, `$(pwd` → `pwd`, `` `git `` → `git`. */
function stripOpener(token: string): string {
  let t = token;
  if (t.startsWith("$(")) t = t.slice(2);
  else if (t.startsWith("(")) t = t.slice(1);
  if (t.startsWith("`")) t = t.slice(1);
  return t;
}

const ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*=/;
const FUNCTION_DEF = /^[A-Za-z_][A-Za-z0-9_]*\(\)?$/;
const PROGRAM = /^[A-Za-z0-9][A-Za-z0-9._+-]*$/;

/** When an assignment's value is a command substitution (`WT=$(ssh …)`), return
 * the inner program candidate (`$(ssh` → `ssh`); otherwise null (plain value). */
function substitutionProgram(token: string): string | null {
  const rhs = token.slice(token.indexOf("=") + 1);
  if (rhs.startsWith("$((")) return null; // arithmetic `$((…))`, not a command
  if (rhs.startsWith("$(") || rhs.startsWith("`")) {
    const inner = stripOpener(rhs);
    return inner || null;
  }
  return null;
}

/** True when a basename is a real-looking program (not a fragment, flag, data
 * file, hostname, or redaction token). */
function looksLikeProgram(cand: string): boolean {
  if (!PROGRAM.test(cand)) return false;
  if (/^\d+$/.test(cand)) return false; // a bare number is a fragment, not a program
  // ≥2 dots ⇒ hostname/qualified name (`acme-x.fly.dev`), not a program.
  if ((cand.match(/\./g)?.length ?? 0) >= 2) return false;
  const lower = cand.toLowerCase();
  return !DATA_EXTENSIONS.some((ext) => lower.endsWith(ext));
}

/** Scan one statement for its leading program. Returns the program (a real
 * command) or a builtin (a statement-consuming shell builtin, used as fallback). */
function scanStatement(stmt: string): { program?: string; builtin?: string } {
  for (const tok of stmt.split(/\s+/).filter(Boolean)) {
    let t = tok;
    if (BRACKETS.has(t) || FUNCTION_DEF.test(t) || WRAPPERS.has(t)) continue;
    if (ASSIGNMENT.test(t)) {
      const sub = substitutionProgram(t);
      if (sub) t = sub; // WT=$(ssh … → ssh
      else continue; // plain assignment prefix (incl. secrets) → peel
    }
    // leading `(`/`$(`/backtick, then trailing `)`/quote/`;` junk, then path.
    const cand = basename(stripOpener(t).replace(/[)"'`;,]+$/, "")).toLowerCase();
    if (!cand || cand.startsWith("-")) break; // a flag ⇒ no program here
    if (NOISE_BUILTINS.has(cand)) return { builtin: cand }; // rest are its args
    if (looksLikeProgram(cand)) return { program: cand };
    break; // unparseable fragment ⇒ next statement
  }
  return {};
}

/**
 * The normalized `executable` for a shell command — the leading meaningful
 * program's basename (lowercased), a bare statement-builtin when that's all the
 * command did, or {@link OTHER_BUCKET}. Never returns an argument, assignment
 * value, secret, or raw fragment. See the module doc for the full spec.
 */
export function extractExecutable(command: string): string {
  let fallback: string | null = null;
  for (const raw of splitStatements(command)) {
    const stmt = raw.trim();
    if (!stmt || stmt.startsWith("#")) continue;
    const { program, builtin } = scanStatement(stmt);
    if (program) {
      return program.length > MAX_EXECUTABLE_CHARS ? OTHER_BUCKET : program;
    }
    if (builtin && !fallback) fallback = builtin;
  }
  return fallback ?? OTHER_BUCKET;
}
