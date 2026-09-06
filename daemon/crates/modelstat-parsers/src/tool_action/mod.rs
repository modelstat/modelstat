//! On-device, deterministic structural extraction for one tool call.
//!
//! Produces ONLY the cheap, generic facts + compliance-redacted input; the
//! semantic fields (`action`/`object`/`keywords`/`abstract`/`qualifiers`) are
//! left null/empty on purpose — the backend derives those from retained evidence
//! with a better, re-runnable model.
//!
//! PRIVACY: `input_redacted` retains the complete supplied invocation after
//! [`redact`]; `command_redacted` remains the derived shell-command fact. Raw
//! input never leaves this function.

mod executable;
mod scripts;

pub use executable::{extract_executable, OTHER_BUCKET};
pub use scripts::{detect_script_refs, resolve_script_path, script_candidates};

use modelstat_redact::redact;
use modelstat_wire::{clamp_utf8_bytes, param_shape, ToolAction};
use serde_json::Value;

/// Derived param-shape guard, mirrored from the backend.
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
    let (input_redacted, input_format, mut input_truncated) = retain_input(call.input, call.cwd);

    if let Some(cmd) = &command {
        executable = Some(extract_executable(cmd));
        let args = cmd.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
        let ps = clamp_chars(&param_shape(&args), MAX_FIELD_CHARS);
        param_shape_out = if ps.is_empty() { None } else { Some(ps) };
        let raw_redacted = redact(cmd, call.cwd).text;
        input_truncated |= raw_redacted.len() > modelstat_wire::caps::CONTENT_EXCERPT_MAX;
        let red = clamp_utf8_bytes(&raw_redacted, modelstat_wire::caps::CONTENT_EXCERPT_MAX);
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
        input_redacted,
        input_format,
        input_truncated,
        scripts: Vec::new(),
        confidence: 0.0,
        // Per-surface provenance. shell bumped to v3 (normalized executable);
        // builtin/mcp extraction is unchanged → still v1.
        extractor: format!("{surface}.{}", if surface == "shell" { "v3" } else { "v1" }),
    }
}

/// Retain the complete invocation input without interpreting it. `null` means
/// no supplied input; strings remain text and every other JSON value is encoded
/// once. The privacy floor runs before the UTF-8 byte guard.
fn retain_input(input: &Value, cwd: Option<&str>) -> (Option<String>, Option<String>, bool) {
    let (raw, format) = match input {
        Value::Null => return (None, None, false),
        Value::String(s) => (s.clone(), "text"),
        value => (
            serde_json::to_string(value).expect("serde_json::Value always serializes"),
            "json",
        ),
    };
    let redacted = redact(&raw, cwd).text;
    let truncated = redacted.len() > modelstat_wire::caps::CONTENT_EXCERPT_MAX;
    (
        Some(clamp_utf8_bytes(
            &redacted,
            modelstat_wire::caps::CONTENT_EXCERPT_MAX,
        )),
        Some(format.to_string()),
        truncated,
    )
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
/// shell-style call. An observed object `command`/`cmd` field is the evidence;
/// its value may be a string or argv array. Tool names and source text are not.
fn shell_command_of(input: &Value) -> Option<String> {
    match input {
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
        assert_eq!(
            ta.input_redacted.as_deref(),
            Some(r#"{"plan":[{"step":"do it","status":"pending"}]}"#)
        );
        assert_eq!(ta.input_format.as_deref(), Some("json"));
        assert!(!ta.input_truncated);
        assert_eq!(ta.extractor, "builtin.v1");
    }

    #[test]
    fn raw_string_inputs_remain_builtins_without_local_context() {
        for (name, raw) in [
            ("javascript", "const widgetCount = 3;"),
            (
                "freeform_patch",
                "*** Begin Patch\n*** Add File: example.txt\n+hello\n*** End Patch",
            ),
            ("annotate", "Summarize the fictional widget record"),
        ] {
            let input = json!(raw);
            let call = ToolActionInput {
                server: "builtin",
                name,
                input: &input,
                cwd: None,
            };
            let action = extract_tool_action(&call);
            assert_eq!(action.surface, "builtin", "{name}");
            assert_eq!(action.executable.as_deref(), Some(name), "{name}");
            assert_eq!(action.command_redacted, None, "{name}");
            assert_eq!(action.input_redacted.as_deref(), Some(raw), "{name}");
            assert_eq!(action.input_format.as_deref(), Some("text"), "{name}");
            assert!(!action.input_truncated, "{name}");
            assert_eq!(action.extractor, "builtin.v1", "{name}");
            assert_eq!(extract_local_tool_context(&call), None, "{name}");
        }
    }

    #[test]
    fn mcp_code_input_is_retained_without_becoming_shell() {
        let input = json!({ "language": "javascript", "code": "const total = 3;" });
        let action = extract_tool_action(&ToolActionInput {
            server: "mcp:example",
            name: "evaluate",
            input: &input,
            cwd: None,
        });
        assert_eq!(action.surface, "mcp");
        assert_eq!(action.command_redacted, None);
        assert_eq!(
            action.input_redacted.as_deref(),
            Some(r#"{"language":"javascript","code":"const total = 3;"}"#)
        );
        assert_eq!(action.input_format.as_deref(), Some("json"));
        assert!(!action.input_truncated);
    }

    #[test]
    fn retained_input_is_floored_and_reports_truncation() {
        for (private, format) in [
            (
                json!("notify dev@example.test with Bearer abcdefghijklmnopqrstuvwxyz123456"),
                "text",
            ),
            (
                json!({
                    "recipient": "dev@example.test",
                    "authorization": "Bearer abcdefghijklmnopqrstuvwxyz123456"
                }),
                "json",
            ),
        ] {
            let redacted = extract_tool_action(&ToolActionInput {
                server: "builtin",
                name: "annotate",
                input: &private,
                cwd: None,
            });
            let kept = redacted.input_redacted.as_deref().unwrap();
            assert!(!kept.contains("dev@example.test"));
            assert!(!kept.contains("abcdefghijklmnopqrstuvwxyz123456"));
            assert_eq!(redacted.input_format.as_deref(), Some(format));
            assert!(!redacted.input_truncated);
        }

        let complete = json!("x".repeat(24_173));
        let retained = extract_tool_action(&ToolActionInput {
            server: "builtin",
            name: "annotate",
            input: &complete,
            cwd: None,
        });
        assert_eq!(retained.input_redacted.as_deref(), complete.as_str());
        assert!(!retained.input_truncated);

        let long = json!("€".repeat(modelstat_wire::caps::CONTENT_EXCERPT_MAX));
        let truncated = extract_tool_action(&ToolActionInput {
            server: "builtin",
            name: "annotate",
            input: &long,
            cwd: None,
        });
        assert!(
            truncated.input_redacted.as_deref().unwrap().len()
                <= modelstat_wire::caps::CONTENT_EXCERPT_MAX
        );
        assert!(truncated.input_redacted.as_deref().unwrap().ends_with('€'));
        assert!(truncated.input_truncated);
    }

    #[test]
    fn structured_command_fields_are_shell_evidence() {
        for input in [
            json!({ "command": "printf widget" }),
            json!({ "cmd": ["printf", "widget"] }),
        ] {
            let call = ToolActionInput {
                server: "builtin",
                name: "run",
                input: &input,
                cwd: None,
            };
            let action = extract_tool_action(&call);
            assert_eq!(action.surface, "shell");
            assert_eq!(action.executable.as_deref(), Some("printf"));
            assert!(extract_local_tool_context(&call).is_some());
        }
    }

    #[test]
    fn long_native_command_is_retained_complete() {
        let command = format!("printf {}", "x".repeat(24_173));
        let input = json!({ "command": command });
        let action = extract_tool_action(&ToolActionInput {
            server: "builtin",
            name: "run",
            input: &input,
            cwd: None,
        });
        assert_eq!(action.command_redacted.as_deref(), Some(command.as_str()));
        assert!(!action.input_truncated);
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
