//! UTF-8 byte clamping — port of `packages/daemon-core/src/http/clamp.ts`.
//!
//! The wire boundary clamps every bounded string to its schema cap measured in
//! UTF-8 BYTES, cutting only on a code-point boundary. This is provably safe for
//! whatever unit the server counts in, because for any string
//! `byte_len >= utf16_units >= code_points`, so `<= N bytes` implies `<= N` of
//! either. Driving it off declared caps (not a hand-listed field set) is what
//! keeps every current and future bounded string covered — the field-by-field
//! version is exactly what shipped the CJK 400 bug (§17.2).

/// Truncate `s` so its UTF-8 encoding is `<= max_bytes`, never splitting a
/// character. Returns `s` unchanged when it already fits (ASCII fast path). No
/// ellipsis — this is invisible wire plumbing and a marker would corrupt
/// structured fields (slugs, urls). Port of `clampUtf8Bytes`.
pub fn clamp_utf8_bytes(s: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut out = String::with_capacity(max_bytes);
    let mut bytes = 0usize;
    // `chars()` iterates whole code points; a character is kept or dropped
    // atomically, so the cut is never mid-character (matches JS `for…of`).
    for ch in s.chars() {
        let b = ch.len_utf8();
        if bytes + b > max_bytes {
            break;
        }
        bytes += b;
        out.push(ch);
    }
    out
}

/// Clamp in place: replace `s` with its byte-clamped form only if it overflows.
pub fn clamp_in_place(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        *s = clamp_utf8_bytes(s, max_bytes);
    }
}

/// Clamp an optional bounded string.
pub fn clamp_opt(s: &mut Option<String>, max_bytes: usize) {
    if let Some(v) = s {
        clamp_in_place(v, max_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_fast_path_is_identity() {
        assert_eq!(clamp_utf8_bytes("hello", 320), "hello");
        assert_eq!(clamp_utf8_bytes("hello", 5), "hello");
    }

    #[test]
    fn zero_max_is_empty() {
        assert_eq!(clamp_utf8_bytes("anything", 0), "");
    }

    #[test]
    fn cuts_on_codepoint_boundary_never_mid_char() {
        // Each CJK char is 3 UTF-8 bytes. Cap 7 bytes ⇒ 2 chars (6 bytes) kept,
        // the third would push to 9 > 7 so it's dropped — never a half char.
        let s = "字字字字"; // 12 bytes
        let out = clamp_utf8_bytes(s, 7);
        assert_eq!(out, "字字");
        assert_eq!(out.len(), 6);
        assert!(out.len() <= 7);
    }

    #[test]
    fn astral_char_kept_atomically() {
        // 😀 is 4 UTF-8 bytes. Cap 3 ⇒ dropped entirely (not a split surrogate).
        assert_eq!(clamp_utf8_bytes("😀", 3), "");
        assert_eq!(clamp_utf8_bytes("😀", 4), "😀");
    }
}
