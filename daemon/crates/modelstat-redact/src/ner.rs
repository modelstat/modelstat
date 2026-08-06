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

// Consumed by the `candle` runtime below; without that feature there is no
// inference to window, and the tests still hold the arithmetic honest.
#[cfg_attr(not(feature = "candle"), allow(dead_code))]
/// Learned positions in a BERT-base checkpoint. Not a tuning knob — ask for
/// position 512 and the embedding lookup errors out, which is exactly how long
/// turns came back unclassified.
const MAX_POSITIONS: usize = 512;
/// Tokens per forward pass. Two positions go to the `[CLS]`/`[SEP]` frame.
#[cfg_attr(not(feature = "candle"), allow(dead_code))]
const WINDOW: usize = MAX_POSITIONS - 2;
/// Tokens two neighbouring windows share, so an entity sitting on a seam is whole
/// inside at least one window. 64 wordpieces is far more than any person or
/// organisation name needs, and costs ~13% more passes.
#[cfg_attr(not(feature = "candle"), allow(dead_code))]
const OVERLAP: usize = 64;

/// The windows a `len`-token sequence gets classified in — `(start, end)` halves
/// of a range each.
///
/// Pure and always compiled, because the property that matters is arithmetic, not
/// inference: every token must land in some window. A gap here means a silently
/// unscrubbed stretch of a turn, which is the whole bug this replaced, so it is
/// tested where the model is not.
#[cfg_attr(not(feature = "candle"), allow(dead_code))]
fn plan_windows(len: usize, window: usize, overlap: usize) -> Vec<(usize, usize)> {
    if len == 0 || window == 0 {
        return Vec::new();
    }
    let stride = window.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut start = 0usize;
    loop {
        let end = (start + window).min(len);
        out.push((start, end));
        if end == len {
            return out;
        }
        start += stride;
    }
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
#[cfg(feature = "candle")]
mod candle_ner {
    use std::collections::HashMap;
    use std::path::Path;

    use candle_core::{Device, Tensor, D};
    use candle_nn::{Linear, Module, VarBuilder};
    use candle_transformers::models::bert::{BertModel, Config, DTYPE};
    use tokenizers::Tokenizer;

    use super::{NerModel, NerToken};

    use super::{plan_windows, OVERLAP, WINDOW};

    /// A BERT token-classification model (dslim/bert-base-NER class) over candle
    /// (CPU). Loaded from a model dir with `config.json`, `tokenizer.json`,
    /// `model.safetensors` (HF keys `bert.*` + `classifier.*`).
    pub struct CandleNer {
        model: BertModel,
        classifier: Linear,
        tokenizer: Tokenizer,
        id2label: HashMap<usize, String>,
        device: Device,
        /// The framing token ids, resolved once at load. Each window is framed
        /// itself, because the text is tokenized without them (see
        /// [`CandleNer::try_classify`]).
        cls_id: u32,
        sep_id: u32,
    }

    impl CandleNer {
        pub fn load(model_dir: &Path) -> Result<Self, String> {
            let device = Device::Cpu;
            let cfg_bytes =
                std::fs::read(model_dir.join("config.json")).map_err(|e| e.to_string())?;
            let config: Config = serde_json::from_slice(&cfg_bytes).map_err(|e| e.to_string())?;
            let raw: serde_json::Value =
                serde_json::from_slice(&cfg_bytes).map_err(|e| e.to_string())?;
            let id2label = parse_id2label(&raw)?;
            let num_labels = id2label.len();
            let tokenizer = Tokenizer::from_file(model_dir.join("tokenizer.json"))
                .map_err(|e| e.to_string())?;
            let weights = model_dir.join("model.safetensors");
            // SAFETY: mmap of a trusted, checksum-verified model file we downloaded.
            let vb = unsafe {
                VarBuilder::from_mmaped_safetensors(&[weights], DTYPE, &device)
                    .map_err(|e| e.to_string())?
            };
            let model = BertModel::load(vb.pp("bert"), &config).map_err(|e| e.to_string())?;
            let classifier = candle_nn::linear(config.hidden_size, num_labels, vb.pp("classifier"))
                .map_err(|e| e.to_string())?;
            // Loud rather than defaulted: a checkpoint whose vocabulary lacks the
            // framing tokens would label every window off by one position.
            let cls_id = tokenizer
                .token_to_id("[CLS]")
                .ok_or("tokenizer has no [CLS] token")?;
            let sep_id = tokenizer
                .token_to_id("[SEP]")
                .ok_or("tokenizer has no [SEP] token")?;
            Ok(Self {
                model,
                classifier,
                tokenizer,
                id2label,
                device,
                cls_id,
                sep_id,
            })
        }

        /// One forward pass over `[CLS] + window + [SEP]`, labelling the window's
        /// tokens. Returns one entity label per token, in window order.
        fn label_window(&self, ids: &[u32]) -> Result<Vec<String>, String> {
            let mut framed = Vec::with_capacity(ids.len() + 2);
            framed.push(self.cls_id);
            framed.extend_from_slice(ids);
            framed.push(self.sep_id);
            let input_ids = Tensor::new(framed.as_slice(), &self.device)
                .and_then(|t| t.unsqueeze(0))
                .map_err(|e| e.to_string())?;
            let token_type_ids = input_ids.zeros_like().map_err(|e| e.to_string())?;
            let hidden = self
                .model
                .forward(&input_ids, &token_type_ids, None)
                .and_then(|h| h.squeeze(0)) // [seq, hidden]
                .map_err(|e| e.to_string())?;
            let logits = self
                .classifier
                .forward(&hidden)
                .map_err(|e| e.to_string())?; // [seq, labels]
            let label_ids: Vec<u32> = logits
                .argmax(D::Minus1)
                .and_then(|a| a.to_vec1::<u32>())
                .map_err(|e| e.to_string())?;
            // Drop the two framing positions; what's left lines up with `ids`.
            Ok(label_ids
                .iter()
                .skip(1)
                .take(ids.len())
                .map(|lid| {
                    self.id2label
                        .get(&(*lid as usize))
                        .cloned()
                        .unwrap_or_else(|| "O".to_string())
                })
                .collect())
        }

        fn try_classify(&self, text: &str) -> Result<Vec<NerToken>, String> {
            // Tokenized ONCE, without the framing tokens, so every token keeps a
            // byte offset into the whole `text` no matter which window labels it.
            let enc = self
                .tokenizer
                .encode(text, false)
                .map_err(|e| e.to_string())?;
            let ids: Vec<u32> = enc.get_ids().to_vec();
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let offsets = enc.get_offsets(); // BYTE offsets into `text`
            let words = enc.get_tokens();

            // WINDOWED, because BERT carries exactly `MAX_POSITIONS` learned
            // positions and asking it for more is an error, not a slower answer.
            // Before turns were captured verbatim this never bit — an abstract fit
            // in one pass — and the failure mode was the worst kind: `classify`
            // returned `None`, which `ner_redact` reads as "no model", so a long
            // turn passed through UNREDACTED. Windowing is not truncation: every
            // token is labelled, just not all in one pass.
            let mut labels: Vec<String> = Vec::with_capacity(ids.len());
            for (start, end) in plan_windows(ids.len(), WINDOW, OVERLAP) {
                let window = self.label_window(&ids[start..end])?;
                // Keep the EARLIER window's answer for the overlap: it saw those
                // tokens with more left context, and an entity that straddles a
                // seam is whole in exactly that window.
                let fresh = labels.len().saturating_sub(start);
                labels.extend(window.into_iter().skip(fresh));
            }

            let mut out = Vec::with_capacity(labels.len());
            for (i, entity) in labels.into_iter().enumerate() {
                let (sb, eb) = offsets.get(i).copied().unwrap_or((0, 0));
                out.push(NerToken {
                    entity,
                    word: words.get(i).cloned().unwrap_or_default(),
                    start: Some(byte_to_char(text, sb)),
                    end: Some(byte_to_char(text, eb)),
                });
            }
            Ok(out)
        }
    }

    impl NerModel for CandleNer {
        fn classify(&self, text: &str) -> Option<Vec<NerToken>> {
            self.try_classify(text).ok()
        }
    }

    /// Char index for a byte offset (the NER splice is char-based; candle
    /// tokenizers report byte offsets).
    fn byte_to_char(text: &str, byte: usize) -> usize {
        let b = byte.min(text.len());
        text[..b].chars().count()
    }

    fn parse_id2label(raw: &serde_json::Value) -> Result<HashMap<usize, String>, String> {
        let map = raw
            .get("id2label")
            .and_then(|v| v.as_object())
            .ok_or_else(|| "config.json missing id2label".to_string())?;
        let mut out = HashMap::new();
        for (k, v) in map {
            let id: usize = k
                .parse()
                .map_err(|_| "non-numeric id2label key".to_string())?;
            let label = v.as_str().unwrap_or("O").to_string();
            out.insert(id, label);
        }
        Ok(out)
    }
}

#[cfg(feature = "candle")]
pub use candle_ner::CandleNer;

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
    fn every_token_lands_in_a_window() {
        for len in [0usize, 1, 5, 509, 510, 511, 1_020, 4_097, 65_537] {
            let plan = plan_windows(len, WINDOW, OVERLAP);
            if len == 0 {
                assert!(plan.is_empty());
                continue;
            }
            let mut covered = vec![false; len];
            for &(a, b) in &plan {
                assert!(b > a && b <= len, "bad window {a}..{b} for len {len}");
                assert!(
                    b - a <= WINDOW,
                    "window {a}..{b} exceeds what the model can take"
                );
                for c in covered[a..b].iter_mut() {
                    *c = true;
                }
            }
            assert!(
                covered.iter().all(|c| *c),
                "len {len} left tokens unclassified: {plan:?}"
            );
            // Neighbours must actually overlap, or a name on the seam is half a
            // name to both of them.
            for pair in plan.windows(2) {
                let (prev, next) = (pair[0], pair[1]);
                assert!(
                    next.0 < prev.1,
                    "windows {prev:?} and {next:?} do not overlap"
                );
            }
        }
    }

    #[test]
    fn a_short_sequence_is_one_window_and_a_long_one_is_many() {
        assert_eq!(plan_windows(10, WINDOW, OVERLAP), vec![(0, 10)]);
        assert_eq!(plan_windows(WINDOW, WINDOW, OVERLAP), vec![(0, WINDOW)]);
        // One token past the limit is the case that used to error out entirely.
        let plan = plan_windows(WINDOW + 1, WINDOW, OVERLAP);
        assert_eq!(plan.len(), 2, "{plan:?}");
        assert_eq!(plan[0], (0, WINDOW));
        assert_eq!(plan[1].1, WINDOW + 1);
    }

    #[test]
    fn a_pathological_overlap_still_makes_progress() {
        // overlap >= window would stride by zero and loop forever; the plan
        // clamps instead, because a hang here would stall the whole pipeline.
        let plan = plan_windows(2_000, 100, 100);
        assert!(plan.len() < 3_000);
        assert_eq!(plan.last().unwrap().1, 2_000);
    }

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
}
