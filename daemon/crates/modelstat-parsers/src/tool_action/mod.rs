//! On-device, deterministic structural extraction for one tool call — a port of
//! `packages/parsers/src/tool-action/index.ts`.
//!
//! Produces ONLY the cheap, generic facts + the compliance-redacted command; the
//! semantic fields (`action`/`object`/`keywords`/`abstract`/`qualifiers`) are
//! left null/empty on purpose — the backend derives those from `command_redacted`
//! with a better, re-runnable model.
//!
//! PRIVACY: the only command-derived outputs are `param_shape` (every value
//! masked to `§`) and `command_redacted` (secrets/PII stripped by [`redact`],
//! in-repo paths relativised against `cwd`). The raw command is never returned.

mod executable;
mod scripts;

pub use executable::{extract_executable, OTHER_BUCKET};
pub use scripts::{detect_script_refs, resolve_script_path, script_candidates};

use modelstat_redact::redact;
use modelstat_wire::{param_shape, ToolAction};
use serde_json::Value;

/// Malicious-size guard, mirrored from the backend
/// (`MAX_TOOL_ACTION_PARAM_SHAPE_CHARS` / `MAX_TOOL_ACTION_COMMAND_CHARS`).
const MAX_FIELD_CHARS: usize = 16_384;

/// Truncate to at most `max` Unicode code points (matches the backend's
/// char-boundary clamp; never splits a surrogate pair). JS `[...s]` iterates
/// code points, so this is `chars()`.
fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect()
    } else {
        s.to_string()
    }
}

/// What the parser has in hand for one observed call at draft-build time.
pub struct ToolActionInput<'a> {
    /// `builtin` or `mcp:<server>`.
    pub server: &'a str,
    /// Bare tool name (`Bash`, `create_pr`, `shell`).
    pub name: &'a str,
    /// The raw tool input (a shell `{ command }` object, an args object, …).
    pub input: &'a Value,
    /// The event's cwd, if known — lets redaction keep in-repo paths relative.
    pub cwd: Option<&'a str>,
}

/// Extract the deterministic structural facts for one tool call. The returned
/// [`ToolAction`] is wire-shaped; semantic fields are null/empty (server-derived).
pub fn extract_tool_action(call: &ToolActionInput) -> ToolAction {
    let is_mcp = call.server.starts_with("mcp:");
    let command = if is_mcp {
        None
    } else {
        shell_command_of(call.input)
    };
    let surface = if is_mcp {
        "mcp"
    } else if command.is_some() {
        "shell"
    } else {
        "builtin"
    };

    // executable: `call.name || null`.
    let mut executable = if call.name.is_empty() {
        None
    } else {
        Some(call.name.to_string())
    };
    let mut param_shape_out: Option<String> = None;
    let mut command_redacted: Option<String> = None;

    if let Some(cmd) = &command {
        executable = Some(extract_executable(cmd));
        let args = cmd.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
        let ps = clamp_chars(&param_shape(&args), MAX_FIELD_CHARS);
        param_shape_out = if ps.is_empty() { None } else { Some(ps) };
        let red = clamp_chars(&redact(cmd, call.cwd).text, MAX_FIELD_CHARS);
        command_redacted = if red.is_empty() { None } else { Some(red) };
    }

    ToolAction {
        surface: surface.to_string(),
        executable,
        action: None,
        object: None,
        qualifiers: Vec::new(),
        param_shape: param_shape_out,
        keywords: Vec::new(),
        r#abstract: None,
        command_redacted,
        scripts: Vec::new(),
        confidence: 0.0,
        // Per-surface provenance. shell bumped to v3 (normalized executable);
        // builtin/mcp extraction is unchanged → still v1.
        extractor: format!("{surface}.{}", if surface == "shell" { "v3" } else { "v1" }),
    }
}

/// The local-only context the agent needs to read + summarise a shell call's
/// referenced script files: the RAW command + cwd. Returns None for non-shell
/// calls (mcp/builtin).
///
/// This is the ONLY function that surfaces the raw command; it travels
/// local-only, NEVER on the wire.
pub fn extract_local_tool_context(call: &ToolActionInput) -> Option<(String, Option<String>)> {
    if call.server.starts_with("mcp:") {
        return None;
    }
    let command = shell_command_of(call.input)?;
    Some((command, call.cwd.map(str::to_string)))
}

/// The shell command string inside a tool input, or None when this isn't a
/// shell-style call. Generic: a string input, or a string/argv `command`/`cmd`
/// field — no tool-name allowlist.
fn shell_command_of(input: &Value) -> Option<String> {
    match input {
        Value::String(s) => nonempty(s),
        Value::Object(map) => {
            // `input.command ?? input.cmd` — nullish coalescing: command wins
            // unless absent or JSON null.
            let cmd = match map.get("command") {
                Some(v) if !v.is_null() => Some(v),
                _ => map.get("cmd"),
            };
            match cmd {
                Some(Value::String(s)) => nonempty(s),
                Some(Value::Array(arr)) => {
                    let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                    if parts.is_empty() {
                        None
                    } else {
                        Some(parts.join(" "))
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// `s.trim() ? s : null` — returns the ORIGINAL (untrimmed) string when it has
/// any non-whitespace content.
fn nonempty(s: &str) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shell_action_matches_claude_golden() {
        let input = json!({ "command": "npm test" });
        let ta = extract_tool_action(&ToolActionInput {
            server: "builtin",
            name: "Bash",
            input: &input,
            cwd: Some("/Users/dev/projects/myrepo"),
        });
        assert_eq!(ta.surface, "shell");
        assert_eq!(ta.executable.as_deref(), Some("npm"));
        assert_eq!(ta.param_shape.as_deref(), Some("§"));
        assert_eq!(ta.command_redacted.as_deref(), Some("npm test"));
        assert_eq!(ta.extractor, "shell.v3");
    }

    #[test]
    fn builtin_action_matches_codex_golden() {
        let input = json!({ "plan": [{ "step": "do it", "status": "pending" }] });
        let ta = extract_tool_action(&ToolActionInput {
            server: "builtin",
            name: "update_plan",
            input: &input,
            cwd: None,
        });
        assert_eq!(ta.surface, "builtin");
        assert_eq!(ta.executable.as_deref(), Some("update_plan"));
        assert_eq!(ta.param_shape, None);
        assert_eq!(ta.command_redacted, None);
        assert_eq!(ta.extractor, "builtin.v1");
    }

    #[test]
    fn local_context_only_for_shell() {
        let shell = json!({ "command": "ls" });
        assert!(extract_local_tool_context(&ToolActionInput {
            server: "builtin",
            name: "Bash",
            input: &shell,
            cwd: Some("/x"),
        })
        .is_some());
        let mcp = json!({ "q": "hi" });
        assert!(extract_local_tool_context(&ToolActionInput {
            server: "mcp:github",
            name: "search",
            input: &mcp,
            cwd: None,
        })
        .is_none());
    }
}
