//! Entropy pass — port of the `redact.ts` generic high-entropy catcher. Runs on
//! tokens of ≥32 chars from `[A-Za-z0-9/+=_-]`, then checks slash-delimited
//! components when the whole token is not sensitive.
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

fn classify(candidate: &str) -> Option<&'static str> {
    if candidate.len() < 32 {
        return None;
    }
    if candidate.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some("[REDACTED:hash]");
    }
    if candidate
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    if (candidate.ends_with('=') || candidate.contains('+')) && entropy(candidate) >= 3.5 {
        return Some("[REDACTED:base64]");
    }
    let has_digit = candidate.chars().any(|c| c.is_ascii_digit());
    let has_upper = candidate.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = candidate.chars().any(|c| c.is_ascii_lowercase());
    if has_digit && has_upper && has_lower && entropy(candidate) >= 3.6 {
        return Some("[REDACTED:hi-entropy]");
    }
    None
}

fn redact_candidate(candidate: &str, counts: &mut RedactionCounts) -> String {
    if let Some(replacement) = classify(candidate) {
        counts.secrets_found += 1;
        return replacement.to_string();
    }
    let mut out = String::with_capacity(candidate.len());
    for (index, component) in candidate.split('/').enumerate() {
        if index > 0 {
            out.push('/');
        }
        if let Some(replacement) = classify(component) {
            counts.secrets_found += 1;
            out.push_str(replacement);
        } else {
            out.push_str(component);
        }
    }
    out
}

/// Apply the entropy pass in place over `out`, updating `counts.secrets_found`.
pub(crate) fn apply(out: &str, counts: &mut RedactionCounts) -> String {
    token_candidate()
        .replace_all(out, |caps: &Captures| redact_candidate(&caps[0], counts))
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
