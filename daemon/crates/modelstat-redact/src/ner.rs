//! Redaction layer 2 — the on-device NER adapter (feature §9.5, plan §3/D5).
//!
//! A token-classification model (BERT-base-NER class) names person/org/location
//! entities the deterministic floor (layer 1) can't catch. This module owns the
//! pure post-processing — BIO span merge, the precise offset splice (else the
//! word-boundary surface fallback), the `pf_<type>` counts, and the fail-closed
//! liveness probe — over a [`NerModel`] trait. The candle runtime slots in behind
//! the trait (`candle` feature); until then [`UnavailableNer`] latches
//! pass-through, and [`ner_active`] answers `false` so cloud/self-hosted egress
//! fails CLOSED (holds) rather than shipping less-redacted (§21.5).

use std::collections::BTreeMap;

/// One classified subword token from the NER model.
#[derive(Debug, Clone)]
pub struct NerToken {
    /// The BIO tag, e.g. `"B-PER"`, `"I-ORG"`, or `"O"`.
    pub entity: String,
    /// The subword surface (`"##xyz"` wordpiece continuations kept).
    pub word: String,
    /// Char offset of the token's start in the input, when the model provides it.
    pub start: Option<usize>,
    pub end: Option<usize>,
}

/// A token-classification model. `classify` returns None when the model is
/// unavailable (missing/loading) — the caller latches pass-through, and
/// [`ner_active`] then reports the layer as down (fail-closed for egress).
pub trait NerModel {
    fn classify(&self, text: &str) -> Option<Vec<NerToken>>;
}

/// The fail-closed default: no NER model. Redaction is a pass-through and
/// [`ner_active`] is `false`, so cloud/self-hosted HOLD rather than ship
/// less-redacted (§9.5).
pub struct UnavailableNer;

impl NerModel for UnavailableNer {
    fn classify(&self, _text: &str) -> Option<Vec<NerToken>> {
        None
    }
}

/// The result of a layer-2 pass: redacted text + `pf_<type>` counts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NerRedaction {
    pub text: String,
    /// `pf_<type>` → count (e.g. `pf_per`, `pf_org`).
    pub counts: BTreeMap<String, u64>,
}

/// Strip a leading `[BILUE]-` tag prefix (uppercase, as the model emits it).
fn strip_bio_prefix(ent: &str) -> &str {
    let b = ent.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'B' | b'I' | b'L' | b'U' | b'E') && b[1] == b'-' {
        &ent[2..]
    } else {
        ent
    }
}

fn is_b_tag(ent: &str) -> bool {
    let b = ent.as_bytes();
    b.len() >= 2 && (b[0] == b'B' || b[0] == b'b') && b[1] == b'-'
}

/// Reconstruct an entity's surface text from its subwords (`##` continuations
/// concatenate, others space-join). Port of `reconstructSurface`.
fn reconstruct_surface(words: &[&str]) -> String {
    let mut s = String::new();
    for w in words {
        if let Some(rest) = w.strip_prefix("##") {
            s.push_str(rest);
        } else {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(w);
        }
    }
    s.trim().to_string()
}

struct Span<'a> {
    type_: String,
    tokens: Vec<&'a NerToken>,
}

/// Run the layer-2 NER redactor. On an unavailable/erroring model the text is
/// returned unchanged (the floor already redacted it). Port of
/// `redactWithPrivacyFilter`.
pub fn ner_redact<M: NerModel>(model: &M, text: &str) -> NerRedaction {
    if text.is_empty() {
        return NerRedaction {
            text: text.to_string(),
            counts: BTreeMap::new(),
        };
    }
    let Some(tokens) = model.classify(text) else {
        return NerRedaction {
            text: text.to_string(),
            counts: BTreeMap::new(),
        };
    };

    // Decode BIO tags into entity spans.
    let mut spans: Vec<Span> = Vec::new();
    for t in &tokens {
        let ent = t.entity.as_str();
        if ent.is_empty() || ent == "O" || ent == "0" {
            continue;
        }
        let type_ = strip_bio_prefix(ent).to_uppercase();
        let extend = matches!(spans.last(), Some(last) if last.type_ == type_ && !is_b_tag(ent));
        if extend {
            spans.last_mut().unwrap().tokens.push(t);
        } else {
            spans.push(Span {
                type_,
                tokens: vec![t],
            });
        }
    }

    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let bump = |type_: &str, n: u64, counts: &mut BTreeMap<String, u64>| {
        *counts.entry(format!("pf_{}", type_.to_lowercase())).or_insert(0) += n;
    };

    let have_offsets = !spans.is_empty()
        && spans.iter().all(|s| {
            s.tokens
                .iter()
                .all(|t| matches!((t.start, t.end), (Some(a), Some(b)) if b > a))
        });

    if have_offsets {
        // Precise: redact exactly the detected span, right-to-left so each splice
        // keeps the remaining offsets valid.
        let mut ranges: Vec<(String, usize, usize)> = spans
            .iter()
            .map(|s| {
                let start = s.tokens.iter().map(|t| t.start.unwrap()).min().unwrap();
                let end = s.tokens.iter().map(|t| t.end.unwrap()).max().unwrap();
                (s.type_.clone(), start, end)
            })
            .collect();
        ranges.sort_by_key(|r| std::cmp::Reverse(r.1));
        let mut chars: Vec<char> = text.chars().collect();
        for (type_, start, end) in ranges {
            if end > chars.len() || start > end {
                continue; // defensive: a bad offset never panics
            }
            bump(&type_, 1, &mut counts);
            let marker: Vec<char> = format!("[REDACTED:{type_}]").chars().collect();
            chars.splice(start..end, marker);
        }
        NerRedaction {
            text: chars.into_iter().collect(),
            counts,
        }
    } else {
        // No offsets: reconstruct each entity's surface and redact every
        // word-boundary occurrence, longest surface first.
        let mut order: Vec<String> = Vec::new();
        let mut surfaces: BTreeMap<String, String> = BTreeMap::new();
        for s in &spans {
            let words: Vec<&str> = s.tokens.iter().map(|t| t.word.as_str()).collect();
            let surface = reconstruct_surface(&words);
            if !surface.is_empty() && !surfaces.contains_key(&surface) {
                order.push(surface.clone());
                surfaces.insert(surface, s.type_.clone());
            }
        }
        order.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
        let mut out = text.to_string();
        for surface in order {
            let type_ = &surfaces[&surface];
            let (replaced, n) =
                word_boundary_replace(&out, &surface, &format!("[REDACTED:{type_}]"));
            out = replaced;
            if n > 0 {
                bump(type_, n, &mut counts);
            }
        }
        NerRedaction { text: out, counts }
    }
}

/// Replace every word-boundary occurrence of `surface` (not flanked by a
/// letter/digit) with `marker`. Manual boundary check — Rust's `regex` has no
/// lookbehind for the `(?<![\p{L}\p{N}])…(?![\p{L}\p{N}])` the TS used.
fn word_boundary_replace(text: &str, surface: &str, marker: &str) -> (String, u64) {
    let tc: Vec<char> = text.chars().collect();
    let sc: Vec<char> = surface.chars().collect();
    if sc.is_empty() {
        return (text.to_string(), 0);
    }
    let mut out = String::new();
    let mut count = 0u64;
    let mut i = 0usize;
    while i < tc.len() {
        if i + sc.len() <= tc.len() && tc[i..i + sc.len()] == sc[..] {
            let before_ok = i == 0 || !tc[i - 1].is_alphanumeric();
            let after_ok = i + sc.len() >= tc.len() || !tc[i + sc.len()].is_alphanumeric();
            if before_ok && after_ok {
                out.push_str(marker);
                i += sc.len();
                count += 1;
                continue;
            }
        }
        out.push(tc[i]);
        i += 1;
    }
    (out, count)
}

/// Prove the NER layer is LIVE (not a silent pass-through) — the fail-closed gate
/// for cloud/self-hosted (§9.5). A sentinel PERSON that the regex floor does NOT
/// target must come back scrubbed. Never panics — a dead model answers `false`.
pub fn ner_active<M: NerModel>(model: &M) -> bool {
    let sentinel = "Escalate the incident to Katherine Johnson at Globex Corporation.";
    !ner_redact(model, sentinel).text.contains("Katherine Johnson")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canned NER model keyed to the sentinel + a couple fixtures.
    struct MockNer;
    impl NerModel for MockNer {
        fn classify(&self, text: &str) -> Option<Vec<NerToken>> {
            let tok = |ent: &str, word: &str, start: usize, end: usize| NerToken {
                entity: ent.into(),
                word: word.into(),
                start: Some(start),
                end: Some(end),
            };
            // Offsets computed against the sentinel text.
            if let Some(k) = text.find("Katherine Johnson") {
                let g = text.find("Globex Corporation").unwrap();
                return Some(vec![
                    tok("B-PER", "Katherine", k, k + 9),
                    tok("I-PER", "Johnson", k + 10, k + 17),
                    tok("B-ORG", "Globex", g, g + 6),
                    tok("I-ORG", "Corporation", g + 7, g + 18),
                ]);
            }
            Some(vec![])
        }
    }

    #[test]
    fn precise_splice_merges_bio_and_counts() {
        let text = "Escalate the incident to Katherine Johnson at Globex Corporation.";
        let out = ner_redact(&MockNer, text);
        assert_eq!(
            out.text,
            "Escalate the incident to [REDACTED:PER] at [REDACTED:ORG]."
        );
        assert_eq!(out.counts.get("pf_per"), Some(&1));
        assert_eq!(out.counts.get("pf_org"), Some(&1));
    }

    #[test]
    fn liveness_gate() {
        assert!(ner_active(&MockNer));
        // Fail-closed: no model → sentinel survives → NOT active.
        assert!(!ner_active(&UnavailableNer));
    }

    #[test]
    fn unavailable_is_passthrough() {
        let out = ner_redact(&UnavailableNer, "Katherine Johnson");
        assert_eq!(out.text, "Katherine Johnson");
        assert!(out.counts.is_empty());
    }

    #[test]
    fn word_boundary_fallback_keeps_marketing_intact() {
        struct NoOffsets;
        impl NerModel for NoOffsets {
            fn classify(&self, _t: &str) -> Option<Vec<NerToken>> {
                Some(vec![NerToken {
                    entity: "B-PER".into(),
                    word: "Mark".into(),
                    start: None,
                    end: None,
                }])
            }
        }
        let out = ner_redact(&NoOffsets, "Mark reviewed the Marketing plan for Mark");
        assert_eq!(out.text, "[REDACTED:PER] reviewed the Marketing plan for [REDACTED:PER]");
        assert_eq!(out.counts.get("pf_per"), Some(&2));
    }

    #[test]
    fn reconstruct_surface_handles_wordpieces() {
        assert_eq!(reconstruct_surface(&["Kath", "##erine", "Johnson"]), "Katherine Johnson");
    }
}
