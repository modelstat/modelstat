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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Strip a leading `[BILUES]-` tag prefix (uppercase, as the model emits it).
///
/// `S` is the BIOES single — one token that is a whole entity. Privacy Filter emits
/// it constantly (`S-private_email`, `S-secret`), and without it here the prefix
/// survives into the marker: `[REDACTED:S-PRIVATE_EMAIL]`, and every single-token
/// entity reads as a different type from its multi-token twin.
fn strip_bio_prefix(ent: &str) -> &str {
    let b = ent.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'B' | b'I' | b'L' | b'U' | b'E' | b'S') && b[1] == b'-' {
        &ent[2..]
    } else {
        ent
    }
}

/// True for a tag that STARTS a span — `B-` and, for BIOES, `S-` (a single-token
/// entity). Without `S-` here, two neighbouring singles of the same type merge into
/// one span covering the gap between them, redacting whatever sat in between.
fn is_b_tag(ent: &str) -> bool {
    let b = ent.as_bytes();
    b.len() >= 2 && matches!(b[0], b'B' | b'b' | b'S' | b's') && b[1] == b'-'
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

/// Run the layer-2 NER redactor, or `None` when the model did not answer for
/// THIS text. Egress paths must use this one: a turn the model couldn't classify
/// has not been scrubbed, and the only safe thing to do with it is hold.
///
/// [`ner_active`] is not enough on its own — it probes with one short sentinel,
/// so it says the layer is up while individual turns still fail. That gap shipped
/// long turns unredacted once turns became verbatim, which is why the answer is
/// now per text.
pub fn ner_redact_checked<M: NerModel>(model: &M, text: &str) -> Option<NerRedaction> {
    if text.is_empty() {
        return Some(NerRedaction {
            text: text.to_string(),
            counts: BTreeMap::new(),
        });
    }
    model
        .classify(text)
        .map(|tokens| redact_spans(text, tokens))
}

/// Run the layer-2 NER redactor. On an unavailable/erroring model the text is
/// returned unchanged (the floor already redacted it) — fine for LOCAL use, where
/// nothing leaves the machine. Anything that ships must use
/// [`ner_redact_checked`] and hold instead. Port of `redactWithPrivacyFilter`.
pub fn ner_redact<M: NerModel>(model: &M, text: &str) -> NerRedaction {
    ner_redact_checked(model, text).unwrap_or_else(|| NerRedaction {
        text: text.to_string(),
        counts: BTreeMap::new(),
    })
}

/// Splice `[REDACTED:<TYPE>]` over every entity span the model named.
fn redact_spans(text: &str, tokens: Vec<NerToken>) -> NerRedaction {
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
        *counts
            .entry(format!("pf_{}", type_.to_lowercase()))
            .or_insert(0) += n;
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
        let chars_in: Vec<char> = text.chars().collect();
        let ranges: Vec<(String, usize, usize)> = spans
            .iter()
            .map(|s| {
                let start = s.tokens.iter().map(|t| t.start.unwrap()).min().unwrap();
                let end = s.tokens.iter().map(|t| t.end.unwrap()).max().unwrap();
                (s.type_.clone(), start, end)
            })
            .collect();
        // Snapped OUTWARD to whole words, then merged. A model labels SUBWORDS, and
        // splicing one of them leaves the rest of the word sitting in the output:
        // prod shipped `eRPC` as `[REDACTED:ORG]PC` and `Bugbot` as
        // `[REDACTED:ORG]ugbot`. That reads as corruption, but the privacy version
        // is worse — a half-redacted name leaks the other half, e.g. `Katherine` →
        // `[REDACTED:PER]erine`. Redacting the whole word is both honest and safe:
        // over-redacting a few extra characters costs nothing, leaving a fragment
        // costs the thing redaction exists to prevent.
        let mut ranges = merge_ranges(snap_ranges(&chars_in, ranges));
        ranges.sort_by_key(|r| std::cmp::Reverse(r.1));
        let mut chars = chars_in;
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

/// Char index for a byte offset — both tokenizer runtimes report byte offsets, and
/// the splice is char-based.
#[cfg_attr(not(feature = "onnx"), allow(dead_code))]
pub(crate) fn byte_to_char(text: &str, byte: usize) -> usize {
    let b = byte.min(text.len());
    text[..b].chars().count()
}

/// Grow each range outward until neither edge sits inside a word.
///
/// "Inside a word" is decided by the neighbouring character being alphanumeric —
/// a plain `char` test, not a pattern language. Underscores and hyphens are left
/// as boundaries on purpose: `sk_live_x` and `first-last` are compounds a reader
/// still recognises once the sensitive part is gone.
fn snap_ranges(chars: &[char], ranges: Vec<(String, usize, usize)>) -> Vec<(String, usize, usize)> {
    ranges
        .into_iter()
        .map(|(type_, mut start, mut end)| {
            end = end.min(chars.len());
            start = start.min(end);
            while start > 0 && chars[start - 1].is_alphanumeric() {
                start -= 1;
            }
            while end < chars.len() && chars[end].is_alphanumeric() {
                end += 1;
            }
            (type_, start, end)
        })
        .collect()
}

/// Fuse ranges that touch or overlap after snapping.
///
/// Snapping can make two spans meet — two subwords of one word, or a person and
/// an org inside one token. Splicing both would corrupt the text a second way
/// (the first splice invalidates the second's offsets), so they become one range.
/// The earlier span's type wins: it is the one the model was most confident began
/// there, and the marker only has room to name one.
fn merge_ranges(mut ranges: Vec<(String, usize, usize)>) -> Vec<(String, usize, usize)> {
    ranges.sort_by_key(|r| (r.1, r.2));
    let mut out: Vec<(String, usize, usize)> = Vec::with_capacity(ranges.len());
    for (type_, start, end) in ranges {
        match out.last_mut() {
            Some(last) if start <= last.2 => last.2 = last.2.max(end),
            _ => out.push((type_, start, end)),
        }
    }
    out
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
    !ner_redact(model, sentinel)
        .text
        .contains("Katherine Johnson")
}

// ── candle BERT-NER runtime (feature `candle`) ───────────────────────────────
//
// Compile-verified against candle-transformers 0.8; run-verification needs the
// downloaded dslim/bert-base-NER weights. Exact-span parity vs transformers.js is
// not required — PROCESSING_VERSION 16 absorbs the runtime swap (plan R2/D13);
// the fail-closed liveness gate (`ner_active`) keeps egress safe regardless.

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
        assert_eq!(
            out.text,
            "[REDACTED:PER] reviewed the Marketing plan for [REDACTED:PER]"
        );
        assert_eq!(out.counts.get("pf_per"), Some(&2));
    }

    #[test]
    fn reconstruct_surface_handles_wordpieces() {
        assert_eq!(
            reconstruct_surface(&["Kath", "##erine", "Johnson"]),
            "Katherine Johnson"
        );
    }

    /// The property the leak came down to: EVERY token has to be classified by
    /// some window. A gap is a silently unscrubbed stretch of a turn.

    #[test]
    fn a_failed_classification_is_distinguishable_from_a_clean_one() {
        // The distinction the leak turned on: "nothing to redact" and "could not
        // read this" both used to come back as unchanged text.
        assert!(ner_redact_checked(&UnavailableNer, "Katherine Johnson").is_none());
        let clean = ner_redact_checked(&NoEntities, "Katherine Johnson").expect("answered");
        assert_eq!(clean.text, "Katherine Johnson");
        // The lossy wrapper still exists for LOCAL use, and still passes through.
        assert_eq!(
            ner_redact(&UnavailableNer, "Katherine Johnson").text,
            "Katherine Johnson"
        );
    }

    /// Answers, finds nothing.
    struct NoEntities;
    impl NerModel for NoEntities {
        fn classify(&self, _text: &str) -> Option<Vec<NerToken>> {
            Some(Vec::new())
        }
    }

    /// A model that labels only PART of a word — exactly what wordpiece models do,
    /// and what shipped mangled text to production.
    struct LabelsSubword {
        /// Char range of the fragment it "detects", and the type it calls it.
        range: (usize, usize),
    }
    impl NerModel for LabelsSubword {
        fn classify(&self, _text: &str) -> Option<Vec<NerToken>> {
            Some(vec![NerToken {
                entity: "B-ORG".into(),
                word: "frag".into(),
                start: Some(self.range.0),
                end: Some(self.range.1),
            }])
        }
    }

    /// The invariant: a marker never sits against an alphanumeric character. If it
    /// does, part of the word survived — cosmetically wrong, and for a name it
    /// leaks the remainder.
    fn no_marker_touches_a_word(out: &str) {
        let chars: Vec<char> = out.chars().collect();
        let marker: Vec<char> = "[REDACTED:".chars().collect();
        for i in 0..chars.len() {
            if chars[i..].starts_with(&marker) {
                if i > 0 {
                    assert!(
                        !chars[i - 1].is_alphanumeric(),
                        "marker starts inside a word: {out}"
                    );
                }
                if let Some(close) = chars[i..].iter().position(|&c| c == ']') {
                    let after = i + close + 1;
                    if after < chars.len() {
                        assert!(
                            !chars[after].is_alphanumeric(),
                            "marker ends inside a word: {out}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_subword_hit_redacts_the_whole_word_never_a_fragment() {
        // The three real cases from production, with the model labelling only the
        // leading fragment the way BERT did.
        for (text, frag) in [
            ("Error: eRPC Request failed", (7usize, 9usize)), // "eR" of eRPC
            ("Reviewed by Bugbot now", (12, 13)),             // "B" of Bugbot
            ("a Compose app is up", (2, 4)),                  // "Co" of Compose
            ("ping Katherine today", (5, 9)),                 // "Kath" of Katherine
        ] {
            let out = ner_redact(&LabelsSubword { range: frag }, text).text;
            no_marker_touches_a_word(&out);
            assert!(
                !out.contains("PC Request") && !out.contains("ugbot") && !out.contains("mpose"),
                "a word fragment survived: {out}"
            );
            assert!(!out.contains("erine"), "half a name survived: {out}");
        }

        // And the whole word really is gone, not just the fragment.
        let out = ner_redact(
            &LabelsSubword { range: (7, 9) },
            "Error: eRPC Request failed",
        )
        .text;
        assert_eq!(out, "Error: [REDACTED:ORG] Request failed");
    }

    #[test]
    fn snapping_stops_at_punctuation_and_whitespace() {
        // It must not swallow the sentence: a span already on word boundaries is
        // left exactly where it was.
        let out = ner_redact(
            &LabelsSubword { range: (7, 11) },
            "Error: eRPC Request failed",
        )
        .text;
        assert_eq!(out, "Error: [REDACTED:ORG] Request failed");
        // Underscore and hyphen stay boundaries, so compounds keep their shape.
        let out = ner_redact(&LabelsSubword { range: (0, 2) }, "abc_def and x").text;
        assert_eq!(out, "[REDACTED:ORG]_def and x");
    }

    #[test]
    fn overlapping_snapped_ranges_fuse_instead_of_corrupting() {
        // Two subwords of ONE word: splicing both would use offsets the first
        // splice already invalidated.
        struct TwoFragments;
        impl NerModel for TwoFragments {
            fn classify(&self, _t: &str) -> Option<Vec<NerToken>> {
                let tok = |a, b| NerToken {
                    entity: "B-PER".into(),
                    word: "f".into(),
                    start: Some(a),
                    end: Some(b),
                };
                Some(vec![tok(5, 7), tok(8, 10)])
            }
        }
        let out = ner_redact(&TwoFragments, "ping Katherine today").text;
        no_marker_touches_a_word(&out);
        assert_eq!(out, "ping [REDACTED:PER] today");
    }

    #[test]
    fn bioes_singles_are_whole_entities_not_a_type_called_s() {
        assert_eq!(strip_bio_prefix("S-private_email"), "private_email");
        assert_eq!(strip_bio_prefix("E-secret"), "secret");
        assert_eq!(strip_bio_prefix("private_email"), "private_email");
        assert!(is_b_tag("S-secret"), "a single starts its own span");
        assert!(is_b_tag("B-secret"));
        assert!(!is_b_tag("I-secret"));
        assert!(!is_b_tag("E-secret"));
    }

    /// Two single-token entities of the same type, with text between them. They must
    /// stay two spans — merging would redact the words in the gap.
    #[test]
    fn two_adjacent_singles_do_not_swallow_what_sits_between_them() {
        struct TwoSingles;
        impl NerModel for TwoSingles {
            fn classify(&self, _t: &str) -> Option<Vec<NerToken>> {
                Some(vec![
                    NerToken {
                        entity: "S-private_person".into(),
                        word: "Ada".into(),
                        start: Some(0),
                        end: Some(3),
                    },
                    NerToken {
                        entity: "S-private_person".into(),
                        word: "Grace".into(),
                        start: Some(12),
                        end: Some(17),
                    },
                ])
            }
        }
        let out = ner_redact(&TwoSingles, "Ada told me Grace shipped it").text;
        assert_eq!(
            out, "[REDACTED:PRIVATE_PERSON] told me [REDACTED:PRIVATE_PERSON] shipped it",
            "the words between two singles must survive"
        );
    }
}
