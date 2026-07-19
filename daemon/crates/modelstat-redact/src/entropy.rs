//! Entropy pass — port of the `redact.ts` generic high-entropy catcher. Runs on
//! unbroken word tokens of ≥32 chars from `[A-Za-z0-9/+=_-]`, after the named
//! patterns so it never double-counts.
//!
//! Two faithfulness points:
//!   * The candidate class is pure ASCII, so code points == UTF-16 units ==
//!     bytes; the JS denominator `s.length` (UTF-16 units) equals `chars().count()`.
//!   * Shannon entropy sums floats in the SAME order JS does — first-occurrence
//!     (JS `Map` insertion) order — so the running total is bit-identical and a
//!     threshold decision can't flip on float non-associativity.

use regex::{Captures, Regex};
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::RedactionCounts;

/// Shannon entropy over `s`'s characters, summed in first-occurrence order.
pub(crate) fn entropy(s: &str) -> f64 {
    let mut order: Vec<char> = Vec::new();
    let mut counts: HashMap<char, usize> = HashMap::new();
    for c in s.chars() {
        counts.entry(c).and_modify(|n| *n += 1).or_insert_with(|| {
            order.push(c);
            1
        });
    }
    let len = s.chars().count() as f64;
    let mut h = 0.0f64;
    for c in &order {
        let n = counts[c] as f64;
        let p = n / len;
        h -= p * p.log2();
    }
    h
}

fn token_candidate() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9/+=_-]{32,}").unwrap())
}

/// Apply the entropy pass in place over `out`, updating `counts.secrets_found`.
pub(crate) fn apply(out: &str, counts: &mut RedactionCounts) -> String {
    token_candidate()
        .replace_all(out, |caps: &Captures| {
            let m = &caps[0];
            // Long pure hex = digest / git SHA / hash — collapse it.
            if m.chars().all(|c| c.is_ascii_hexdigit()) {
                counts.secrets_found += 1;
                return "[REDACTED:hash]".to_string();
            }
            // SCREAMING_SNAKE constant names are not secrets.
            if m.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                return m.to_string();
            }
            // Base64 / base64url blobs: trailing `=` or an embedded `+` are strong
            // binary-payload signals (`/` alone is a path separator, NOT a signal).
            let ends_eq_or_plus = m.ends_with('=') || m.contains('+');
            if ends_eq_or_plus && entropy(m) >= 3.5 {
                counts.secrets_found += 1;
                return "[REDACTED:base64]".to_string();
            }
            // Generic high-entropy token — an API key with no explicit pattern.
            let has_digit = m.chars().any(|c| c.is_ascii_digit());
            let has_upper = m.chars().any(|c| c.is_ascii_uppercase());
            let has_lower = m.chars().any(|c| c.is_ascii_lowercase());
            if !(has_digit && has_upper && has_lower) {
                return m.to_string();
            }
            if entropy(m) < 3.6 {
                return m.to_string();
            }
            counts.secrets_found += 1;
            "[REDACTED:hi-entropy]".to_string()
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_of_uniform_string_is_zero() {
        assert_eq!(entropy("aaaaaaaa"), 0.0);
    }

    #[test]
    fn entropy_rises_with_variety() {
        assert!(entropy("abcdefghijklmnop") > entropy("aaaaaaaaaaaaaaab"));
    }
}
