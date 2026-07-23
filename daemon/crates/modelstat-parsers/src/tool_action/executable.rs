//! Deterministic `executable` normalization for a shell command (`shell.v3`) —
//! a byte-for-byte port of `packages/parsers/src/tool-action/executable.ts`.
//!
//! Finds the *leading meaningful program*: split the command into statements
//! (quote-aware), and for each statement peel the noise (cd/wrappers/env
//! assignments/pipelines) that hides the real program, then return its basename.
//! Emits only a program basename or [`OTHER_BUCKET`] — never an argument,
//! assignment value, secret, or raw fragment. Mirrored server-side in Rust
//! (`modelstat_core::tool_exec`) so ingested rows backfill from `command_redacted`.

use std::sync::OnceLock;

use regex::Regex;

/// Bucket token for shell calls that don't reduce to a single program. Starts
/// with `(`, which a real program basename never can, so it never collides.
pub const OTHER_BUCKET: &str = "(other)";

/// Max executable length (mirrors `executable: z.string().max(80)`).
const MAX_EXECUTABLE_CHARS: usize = 80;

/// Exec wrappers / prefixes whose *next* token is the real program (incl.
/// control-flow openers that immediately precede a command).
fn is_wrapper(t: &str) -> bool {
    matches!(
        t,
        "sudo"
            | "doas"
            | "env"
            | "command"
            | "exec"
            | "builtin"
            | "nohup"
            | "setsid"
            | "time"
            | "nice"
            | "ionice"
            | "chrt"
            | "stdbuf"
            | "xargs"
            | "then"
            | "do"
            | "else"
    )
}

/// Group/subshell brackets to peel (a statement may open inside one).
fn is_bracket(t: &str) -> bool {
    matches!(t, "{" | "}" | "(" | ")")
}

/// Shell builtins / keywords that *consume* their statement: their trailing
/// tokens are their own arguments, not a program. Recorded as a fallback so
/// `cd x && git push` resolves to `git` while a bare `cd ~` stays `cd`.
fn is_noise_builtin(t: &str) -> bool {
    matches!(
        t,
        "cd" | "pushd"
            | "popd"
            | "echo"
            | "printf"
            | "export"
            | "unset"
            | "set"
            | "readonly"
            | "typeset"
            | "declare"
            | "local"
            | "alias"
            | "source"
            | "."
            | "eval"
            | ":"
            | "true"
            | "false"
            | "read"
            | "wait"
            | "trap"
            | "umask"
            | "shift"
            | "return"
            | "getopts"
            | "hash"
            | "let"
            | "test"
            | "["
            | "[["
            | "for"
            | "while"
            | "until"
            | "if"
            | "elif"
            | "fi"
            | "case"
            | "esac"
            | "select"
            | "function"
            | "done"
    )
}

/// Data-file extensions a program basename never legitimately ends in.
const DATA_EXTENSIONS: &[&str] = &[
    ".output", ".txt", ".log", ".json", ".jsonl", ".md", ".csv", ".tmp", ".out", ".git", ".lock",
];

fn assignment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=").unwrap())
}

fn function_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*\(\)?$").unwrap())
}

fn program_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._+-]*$").unwrap())
}

/// Split a command into statements on `;`, `&&`, `||`, `|`, `&`, and newlines,
/// honoring single/double quotes and backslash escapes so separators inside a
/// quoted argument don't split. Brackets are kept inline (peeled per-statement).
fn split_statements(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let n = chars.len();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut single = false;
    let mut double = false;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if single {
            cur.push(c);
            if c == '\'' {
                single = false;
            }
            i += 1;
            continue;
        }
        if double {
            if c == '\\' && i + 1 < n {
                cur.push(c);
                cur.push(chars[i + 1]);
                i += 2;
                continue;
            }
            cur.push(c);
            if c == '"' {
                double = false;
            }
            i += 1;
            continue;
        }
        if c == '\'' {
            single = true;
            cur.push(c);
            i += 1;
            continue;
        }
        if c == '"' {
            double = true;
            cur.push(c);
            i += 1;
            continue;
        }
        if c == '\\' && i + 1 < n {
            cur.push(c);
            cur.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '#' && (cur.is_empty() || cur.ends_with(|ch: char| ch.is_whitespace())) {
            // comment at a word boundary → runs to end of line (drop it, incl. any
            // `;`/`|` inside)
            out.push(std::mem::take(&mut cur));
            while i + 1 < n && chars[i + 1] != '\n' {
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == '\n' || c == ';' {
            out.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        if c == '&' || c == '|' {
            out.push(std::mem::take(&mut cur));
            if i + 1 < n && chars[i + 1] == c {
                i += 1; // collapse && / ||
            }
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    out.push(cur);
    out
}

/// Basename of a path-or-program token: `/usr/bin/git` → `git`, `./d.sh` → `d.sh`.
fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// Strip a leading subshell/substitution opener from a candidate token:
/// `(rg` → `rg`, `$(pwd` → `pwd`, `` `git `` → `git`.
fn strip_opener(token: &str) -> &str {
    let mut t = token;
    if let Some(rest) = t.strip_prefix("$(") {
        t = rest;
    } else if let Some(rest) = t.strip_prefix('(') {
        t = rest;
    }
    if let Some(rest) = t.strip_prefix('`') {
        t = rest;
    }
    t
}

/// When an assignment's value is a command substitution (`WT=$(ssh …)`), return
/// the inner program candidate (`$(ssh` → `ssh`); otherwise None (plain value).
fn substitution_program(token: &str) -> Option<String> {
    let eq = token.find('=')?;
    let rhs = &token[eq + 1..];
    if rhs.starts_with("$((") {
        return None; // arithmetic `$((…))`, not a command
    }
    if rhs.starts_with("$(") || rhs.starts_with('`') {
        let inner = strip_opener(rhs);
        if inner.is_empty() {
            None
        } else {
            Some(inner.to_string())
        }
    } else {
        None
    }
}

/// True when a basename is a real-looking program (not a fragment, flag, data
/// file, hostname, or redaction token).
fn looks_like_program(cand: &str) -> bool {
    if !program_re().is_match(cand) {
        return false;
    }
    if cand.chars().all(|c| c.is_ascii_digit()) {
        return false; // a bare number is a fragment
    }
    if cand.matches('.').count() >= 2 {
        return false; // hostname/qualified name
    }
    let lower = cand.to_lowercase();
    !DATA_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

enum Scan {
    Program(String),
    Builtin(String),
    None,
}

/// Scan one statement for its leading program.
fn scan_statement(stmt: &str) -> Scan {
    for tok in stmt.split_whitespace() {
        let mut t = tok.to_string();
        if is_bracket(&t) || function_def_re().is_match(&t) || is_wrapper(&t) {
            continue;
        }
        if assignment_re().is_match(&t) {
            match substitution_program(&t) {
                Some(sub) => t = sub, // WT=$(ssh … → ssh
                None => continue,     // plain assignment prefix (incl. secrets) → peel
            }
        }
        // leading `(`/`$(`/backtick, then trailing `)`/quote/`;` junk, then path.
        let stripped = strip_opener(&t);
        let trimmed = stripped.trim_end_matches([')', '"', '\'', '`', ';', ',']);
        let cand = basename(trimmed).to_lowercase();
        if cand.is_empty() || cand.starts_with('-') {
            break; // a flag ⇒ no program here
        }
        if is_noise_builtin(&cand) {
            return Scan::Builtin(cand); // rest are its args
        }
        if looks_like_program(&cand) {
            return Scan::Program(cand);
        }
        break; // unparseable fragment ⇒ next statement
    }
    Scan::None
}

/// The normalized `executable` for a shell command — the leading meaningful
/// program's basename (lowercased), a bare statement-builtin when that's all the
/// command did, or [`OTHER_BUCKET`].
pub fn extract_executable(command: &str) -> String {
    let mut fallback: Option<String> = None;
    for raw in split_statements(command) {
        let stmt = raw.trim();
        if stmt.is_empty() || stmt.starts_with('#') {
            continue;
        }
        match scan_statement(stmt) {
            Scan::Program(p) => {
                return if p.len() > MAX_EXECUTABLE_CHARS {
                    OTHER_BUCKET.to_string()
                } else {
                    p
                };
            }
            Scan::Builtin(b) => {
                if fallback.is_none() {
                    fallback = Some(b);
                }
            }
            Scan::None => {}
        }
    }
    fallback.unwrap_or_else(|| OTHER_BUCKET.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_cases() {
        let cases = [
            (
                "kubectl rollout restart deploy/payments-api -n prod",
                "kubectl",
            ),
            ("cd x && git push", "git"),
            ("./deploy.sh --now", "deploy.sh"),
            ("sudo systemctl restart nginx", "systemctl"),
            ("FOO=bar realcmd --flag", "realcmd"),
            ("WT=$(ssh host uptime) && echo $WT", "ssh"),
            ("ls -la", "ls"),
            ("echo hello world", "echo"),
            ("cd ~", "cd"),
            ("# just a comment", "(other)"),
            ("CK=\"sk_live_examplefake0123456789\" node index.js", "node"),
            ("for i in 1 2 3; do curl https://x; done", "curl"),
            ("pnpm -C packages/core test", "pnpm"),
        ];
        for (cmd, want) in cases {
            assert_eq!(extract_executable(cmd), want, "cmd = {cmd:?}");
        }
    }
}
