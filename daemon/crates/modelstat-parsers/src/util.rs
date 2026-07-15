//! Small shared string helpers ported from the TS parsers' excerpt/name paths.

use std::sync::OnceLock;

use regex::Regex;

/// Take at most `max` UTF-16 code units of `s`, matching JS `String.slice(0,max)`
/// (which slices UTF-16 units, not code points). BMP-only inputs slice identically
/// to a char count.
pub fn slice_utf16(s: &str, max: usize) -> String {
    if s.encode_utf16().count() <= max {
        return s.to_string();
    }
    let units: Vec<u16> = s.encode_utf16().take(max).collect();
    String::from_utf16_lossy(&units)
}

/// Strip fenced ``` code blocks ``` and inline `code` spans, replacing each with
/// a single space — JS `text.replace(/```[\s\S]*?```/g," ").replace(/`[^`]*`/g," ")`.
pub fn strip_code(text: &str) -> String {
    static FENCE: OnceLock<Regex> = OnceLock::new();
    static INLINE: OnceLock<Regex> = OnceLock::new();
    let fence = FENCE.get_or_init(|| Regex::new(r"```[\s\S]*?```").unwrap());
    let inline = INLINE.get_or_init(|| Regex::new(r"`[^`]*`").unwrap());
    let a = fence.replace_all(text, " ");
    inline.replace_all(&a, " ").into_owned()
}

/// Collapse every whitespace run to a single space and trim — JS
/// `text.replace(/\s+/g," ").trim()`.
pub fn collapse_ws(text: &str) -> String {
    static WS: OnceLock<Regex> = OnceLock::new();
    let ws = WS.get_or_init(|| Regex::new(r"\s+").unwrap());
    ws.replace_all(text, " ").trim().to_string()
}
