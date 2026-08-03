//! Canonical wire enumerations — a verbatim port of `@modelstat/core/enums`.
//!
//! Order and membership are the contract (an enums fixture generated from the TS
//! pins them in `tests/golden/enums.json`). Wire structs carry these as
//! validated `String`s rather than Rust enums: the set is large and churny
//! (adding a value is cheap, and the daemon must round-trip a value a newer
//! server knows but this build doesn't), so validation is an explicit pass, not
//! a deserialize-time hard fail — mirroring the TS Zod `z.enum(...)`.
//!
//! Parity note: the TS `AGENTS` array has **33** entries. feature §4 prose says
//! "34-enum"; per the spec's own rule ("where docs and code disagree, code is
//! authoritative", §23) the code count wins and this list matches it exactly.

/// AI clients (`agent` field). 33 entries — see the parity note above.
pub const AGENTS: &[&str] = &[
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
    "pi",
    "openclaw",
    "hermes",
    "ollama",
    "raw_sdk_anthropic",
    "raw_sdk_openai",
    "raw_sdk_google",
    "chatgpt_web",
    "claude_web",
    "gemini_web",
    "grok_web",
    "unknown",
];

/// Model providers (`provider` field). 11 entries.
pub const PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "cursor",
    "github",
    "deepseek",
    "moonshot",
    "mistral",
    "xai",
    "ollama_local",
    "unknown",
];

/// Event categories (`kind` field). 5 entries.
pub const EVENT_KINDS: &[&str] = &[
    "user_message",
    "assistant_message",
    "tool_call",
    "tool_result",
    "summary",
];

/// Tool-call outcomes (`status` field). 5 entries.
pub const TOOL_CALL_STATUSES: &[&str] = &["success", "error", "denied", "timeout", "unknown"];

/// OS families (`os_family`). 4 entries — `windows` present (feature §4).
pub const OS_FAMILIES: &[&str] = &["macos", "linux", "windows", "other"];

/// Architectures (`arch`). 3 entries.
pub const ARCHES: &[&str] = &["x86_64", "arm64", "other"];

/// Daemon heartbeat phases (`status`). 9 entries.
pub const DAEMON_PHASES: &[&str] = &[
    "starting",
    "discovering",
    "idle",
    "scanning",
    "processing",
    "uploading",
    "watching",
    "offline",
    "error",
];

/// Install-method classification. 19 entries.
pub const INSTALL_METHODS: &[&str] = &[
    "homebrew",
    "homebrew_cask",
    "npm_global",
    "pnpm_global",
    "yarn_global",
    "bun_global",
    "cargo_install",
    "pipx",
    "app_bundle",
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
];

/// Detected-identity owner scopes. 3 entries.
pub const IDENTITY_OWNER_SCOPES: &[&str] = &["org", "personal", "unassigned"];

/// Classifier confidence bands. 5 entries.
pub const CLASSIFICATION_CONFIDENCE: &[&str] = &["hard", "high", "medium", "low", "unclassified"];

/// Summarizer modes, in menu order (cloud is the install default). Feature §9.
pub const SUMMARIZER_MODES: &[&str] = &["cloud", "local", "self-hosted"];

/// `pricing_mode` values. `unknown` is a real answer — a client stating that it
/// looked at the auth material and could not tell — not the absence of one; the
/// field itself is required on every event.
pub const PRICING_MODES: &[&str] = &["subscription", "api", "unknown"];

macro_rules! validator {
    ($fn:ident, $arr:ident) => {
        #[doc = concat!("True if `v` is a member of [`", stringify!($arr), "`].")]
        pub fn $fn(v: &str) -> bool {
            $arr.contains(&v)
        }
    };
}

validator!(is_valid_agent, AGENTS);
validator!(is_valid_provider, PROVIDERS);
validator!(is_valid_event_kind, EVENT_KINDS);
validator!(is_valid_tool_call_status, TOOL_CALL_STATUSES);
validator!(is_valid_os_family, OS_FAMILIES);
validator!(is_valid_arch, ARCHES);
validator!(is_valid_daemon_phase, DAEMON_PHASES);
validator!(is_valid_install_method, INSTALL_METHODS);
validator!(is_valid_summarizer_mode, SUMMARIZER_MODES);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_match_ts_code() {
        // The numbers the golden enums.json (generated from TS) must also show.
        assert_eq!(AGENTS.len(), 33);
        assert_eq!(PROVIDERS.len(), 11);
        assert_eq!(EVENT_KINDS.len(), 5);
        assert_eq!(TOOL_CALL_STATUSES.len(), 5);
        assert_eq!(OS_FAMILIES.len(), 4);
        assert_eq!(DAEMON_PHASES.len(), 9);
        assert_eq!(INSTALL_METHODS.len(), 19);
    }

    #[test]
    fn no_duplicates() {
        for arr in [AGENTS, PROVIDERS, EVENT_KINDS, INSTALL_METHODS] {
            let mut seen = std::collections::HashSet::new();
            for v in arr {
                assert!(seen.insert(*v), "duplicate enum value {v}");
            }
        }
    }
}
