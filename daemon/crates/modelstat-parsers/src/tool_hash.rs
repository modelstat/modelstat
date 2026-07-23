//! Shared tool_call hashing + identity helpers — a byte-for-byte port of
//! `packages/parsers/src/tool-hash/index.ts`.
//!
//! PRIVACY: these helpers exist so raw tool arguments/results never leave the
//! device. Only one-way sha256 digests and byte lengths are derived here; the
//! caller discards the inputs after hashing (see [`wire::ToolCallWire`]).
//!
//! The `fallbackCallId` helper lives in `modelstat-wire` as
//! [`wire::tc_fallback_id`] (same djb2-64 family as the other ids).

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Sentinel for `signature_hash` when input is not a non-empty object.
pub const SIGNATURE_NONE: &str = "none";

/// The three wire fields derived from a tool call's `input`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgsHashes {
    /// Hex sha256 of `JSON.stringify(input)`; `""` when no input.
    pub args_hash: String,
    /// Hex sha256 of the sorted top-level arg key names joined by `,`; the
    /// literal `none` when input is not a non-empty plain object.
    pub signature_hash: String,
    /// UTF-8 byte length of `JSON.stringify(input)`; 0 when no input.
    pub args_bytes: u64,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Hash a tool_use `input` into the three wire fields. A JSON `null` counts as
/// "no input" (matching TS `undefined`/`null`). `JSON.stringify` order is
/// reproduced by `serde_json`'s `preserve_order` feature, so `args_hash` is
/// byte-identical to the TS digest.
pub fn hash_args(input: &Value) -> ArgsHashes {
    if input.is_null() {
        return ArgsHashes {
            args_hash: String::new(),
            signature_hash: SIGNATURE_NONE.to_string(),
            args_bytes: 0,
        };
    }
    let json = serde_json::to_string(input).unwrap_or_default();
    let args_hash = sha256_hex(json.as_bytes());
    let mut signature_hash = SIGNATURE_NONE.to_string();
    if let Value::Object(map) = input {
        if !map.is_empty() {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            signature_hash = sha256_hex(keys.join(",").as_bytes());
        }
    }
    ArgsHashes {
        args_hash,
        signature_hash,
        args_bytes: json.len() as u64,
    }
}

/// UTF-8 byte length of `JSON.stringify(value)` — for `result_bytes`. A JSON
/// `null` or empty string (unmatched / empty result) → 0.
pub fn json_bytes(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::String(s) if s.is_empty() => 0,
        _ => serde_json::to_string(value)
            .map(|j| j.len() as u64)
            .unwrap_or(0),
    }
}

use crate::util::slice_utf16;

fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// Replace full UUIDs and hex tails of ≥8 chars (that end a name segment) with
/// `<dyn>`. Mirrors the two JS regexes:
///   `UUID_RE = /[0-9a-fA-F]{8}-…-…{12}/g`
///   `HEX_TAIL_RE = /[0-9a-fA-F]{8,}(?=$|[^0-9a-zA-Z])/g`
/// The UUID pass runs first (its dashed groups are individually < 8 chars).
fn replace_dyn(input: &str) -> String {
    // Pass 1: full UUIDs → <dyn>.
    let after_uuid = replace_uuids(input);
    // Pass 2: hex runs of ≥8 that are followed by end-of-string or a
    // non-alphanumeric char.
    replace_hex_tails(&after_uuid)
}

fn replace_uuids(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    // group lengths of a UUID: 8-4-4-4-12 separated by '-'.
    let groups = [8usize, 4, 4, 4, 12];
    while i < n {
        if let Some(end) = match_uuid(&chars, i, &groups) {
            out.push_str("<dyn>");
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn match_uuid(chars: &[char], start: usize, groups: &[usize]) -> Option<usize> {
    let mut i = start;
    for (gi, &len) in groups.iter().enumerate() {
        if gi > 0 {
            if i >= chars.len() || chars[i] != '-' {
                return None;
            }
            i += 1;
        }
        for _ in 0..len {
            if i >= chars.len() || !is_hex(chars[i]) {
                return None;
            }
            i += 1;
        }
    }
    Some(i)
}

fn replace_hex_tails(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < n {
        if is_hex(chars[i]) {
            let mut j = i;
            while j < n && is_hex(chars[j]) {
                j += 1;
            }
            let run_len = j - i;
            // Lookahead: end-of-string OR a non-alphanumeric char. `[^0-9a-zA-Z]`
            // — note a hex run followed by another letter/digit (a longer word)
            // does NOT match (that trailing char would be alphanumeric only if
            // it's a non-hex letter/digit; the run already consumed all hex, so
            // the next char is either a non-hex alnum → no match, or non-alnum →
            // match).
            let boundary_ok = j >= n || {
                let c = chars[j];
                !c.is_ascii_alphanumeric()
            };
            if run_len >= 8 && boundary_ok {
                out.push_str("<dyn>");
            } else {
                for c in &chars[i..j] {
                    out.push(*c);
                }
            }
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Tool-name normalization: NFC, trim, hex/UUID-ish tails of ≥8 chars →
/// `<dyn>`, cap 120 UTF-16 units. Keeps stable vendor identifiers verbatim while
/// collapsing per-session dynamic names.
pub fn normalize_tool_name(raw: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let nfc: String = raw.nfc().collect();
    let cleaned = replace_dyn(nfc.trim());
    slice_utf16(&cleaned, 120)
}

/// Normalize an MCP server name to the wire `mcp:<server>` form: normalized +
/// capped to 116 UTF-16 units so the `mcp:` prefix keeps the whole ≤120.
pub fn mcp_server_name(raw: &str) -> String {
    format!("mcp:{}", slice_utf16(&normalize_tool_name(raw), 116))
}

/// Split an observed tool name into wire `server` + `name`:
/// `mcp__<server>__<tool>` → `{ server: "mcp:<server>", name }`, anything else →
/// `{ server: "builtin", name }`. Both parts normalized.
pub fn split_observed_tool_name(observed: &str) -> (String, String) {
    let trimmed = observed.trim();
    if let Some((server, tool)) = split_mcp(trimmed) {
        return (mcp_server_name(server), normalize_tool_name(tool));
    }
    ("builtin".to_string(), normalize_tool_name(observed))
}

/// Match `^mcp__([^_].*?)__(.+)$` — non-greedy first group so the FIRST `__`
/// (after a non-`_` lead char) splits server from tool.
fn split_mcp(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix("mcp__")?;
    let first = rest.chars().next()?;
    if first == '_' {
        return None;
    }
    // Non-greedy `([^_].*?)__(.+)`: find the earliest `__` at index ≥1 whose
    // tail (`.+`) is non-empty.
    let bytes = rest.as_bytes();
    let mut i = 1;
    while i + 2 <= bytes.len() {
        if bytes[i] == b'_' && bytes.get(i + 1) == Some(&b'_') {
            let server = &rest[..i];
            let tool = &rest[i + 2..];
            if !tool.is_empty() {
                return Some((server, tool));
            }
        }
        i += 1;
    }
    None
}

/// Canonical tool identity: `Bash` for builtins, `mcp:<server>/<tool>` for MCP
/// tools. The aggregate-map key + taxonomy leaf name.
pub fn tool_identity(server: &str, name: &str) -> String {
    if server == "builtin" {
        name.to_string()
    } else {
        format!("{server}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_matches_golden() {
        assert_eq!(normalize_tool_name("  Bash  "), "Bash");
        assert_eq!(normalize_tool_name("café"), "café");
        assert_eq!(
            normalize_tool_name("subscribe_a1b2c3d4e5f6"),
            "subscribe_<dyn>"
        );
        assert_eq!(
            normalize_tool_name("job-550e8400-e29b-41d4-a716-446655440000"),
            "job-<dyn>"
        );
        assert_eq!(normalize_tool_name("create_pr"), "create_pr");
        let long = "x".repeat(300);
        assert_eq!(normalize_tool_name(&long), "x".repeat(120));
    }

    #[test]
    fn split_matches_golden() {
        assert_eq!(
            split_observed_tool_name("mcp__github__create_pr"),
            ("mcp:github".into(), "create_pr".into())
        );
        assert_eq!(
            split_observed_tool_name("mcp__brave-search__web_search"),
            ("mcp:brave-search".into(), "web_search".into())
        );
        assert_eq!(
            split_observed_tool_name("mcp__my_server__tool"),
            ("mcp:my_server".into(), "tool".into())
        );
        assert_eq!(
            split_observed_tool_name("Bash"),
            ("builtin".into(), "Bash".into())
        );
        assert_eq!(
            split_observed_tool_name("WebSearch"),
            ("builtin".into(), "WebSearch".into())
        );
    }

    #[test]
    fn hash_args_shapes() {
        // No input.
        let none = hash_args(&Value::Null);
        assert_eq!(none.args_hash, "");
        assert_eq!(none.signature_hash, "none");
        assert_eq!(none.args_bytes, 0);
        // Single-key object → matches the claude_basic golden.
        let h = hash_args(&json!({ "command": "npm test" }));
        assert_eq!(
            h.args_hash,
            "9178ac939ad908d8fd89e97596cdc0e5e28dee903ccb18dc79d0fb98698cbd14"
        );
        assert_eq!(
            h.signature_hash,
            "5d347fd948b66308f502c3f65c8f7e12ff1c5cf8c760bcdfb188ae1ec7b8b618"
        );
        assert_eq!(h.args_bytes, 22);
    }

    #[test]
    fn json_bytes_edges() {
        assert_eq!(json_bytes(&Value::Null), 0);
        assert_eq!(json_bytes(&json!("")), 0);
        assert_eq!(json_bytes(&json!("ok")), 4);
    }

    #[test]
    fn tool_identity_forms() {
        assert_eq!(tool_identity("builtin", "Bash"), "Bash");
        assert_eq!(
            tool_identity("mcp:github", "create_pr"),
            "mcp:github/create_pr"
        );
    }
}
