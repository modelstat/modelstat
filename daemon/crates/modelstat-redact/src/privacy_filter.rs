//! Redaction layer 2 — the on-device PII detector (feature §9.5).
//!
//! [OpenAI Privacy Filter][pf] (Apache 2.0) run through ONNX Runtime: a
//! bidirectional token classifier that names the eight things we must never ship —
//! person, email, phone, address, account number, url, date, and **secret** — as
//! BIOES spans over the input. It labels; it does not rewrite. That matters here:
//! SPEC 0005 stores turns VERBATIM, so the only safe shape is "tell me the spans
//! and I will splice them myself".
//!
//! It replaced a general-purpose NER model (dslim/bert-base-NER via candle) that
//! was wrong for this job in three ways, all measured on production data:
//!
//!   · it could not see secrets, emails or phone numbers at all — those were the
//!     deterministic floor's job alone;
//!   · its ORG label fired on technical product names, not private data: 27,130 of
//!     162,159 stored messages carried `[REDACTED:ORG]`, 25,931 of them with no
//!     person in sight. Redacting `ClickHouse` costs analytics and buys nothing;
//!   · it cost 40s on a 44k-char turn where this costs 3.6s (11x), because BERT's
//!     512-position limit forces overlapping windows over quadratic attention.
//!
//! Privacy Filter has no ORG or LOC label and that is deliberate — an organisation
//! is not private information.
//!
//! # Why windows are exact here
//!
//! Attention is BANDED at [`BAND`] tokens: a token only ever attends within ±128 of
//! itself. So a window that gives its interior tokens at least [`BAND`] tokens of
//! context on each side computes exactly what the whole-input pass would. Measured:
//! 100.00% label agreement against whole-input at windows of 4096/2048/1024/512.
//! Unlike the BERT path this replaced, chunking is not an approximation traded for
//! speed — it IS the fast path, and [`WINDOW`]/[`OVERLAP`] were chosen by
//! measurement (2048/256 beat both larger windows and smaller ones).
//!
//! # Why CPU
//!
//! CoreML measured **205,651ms against 8,992ms on CPU** for the same input: only 60
//! of the graph's 331 nodes are CoreML-supported, so it thrashes between execution
//! providers. The MoE routing (128 experts, top-4 per token, 50M active params) is
//! why — it is control flow, not one big matmul. Do not "optimise" this to a GPU
//! provider without measuring; that is the whole story of this comment.
//!
//! # Why prepacking is off, and why nothing else is
//!
//! A user reported the daemon holding ~4 GB. It was real: this session reached
//! **3.37 GB resident and stayed there**, because ONNX Runtime's defaults assume a
//! server saturating a box with back-to-back inference, and we are a background
//! process that redacts in bursts and then sleeps for minutes.
//!
//! Four candidate knobs were measured on the real weights, same six-turn corpus
//! (37 → 8,797 tokens), M4 Pro, release build, three runs each. Resident memory
//! once the run settled:
//!
//!     setting                    idle RSS     inference
//!     (defaults)                   3.37 GB        4.9 s
//!     prepacking off               2.55 GB        4.3 s   ← shipped
//!     CPU arena off                4.09 GB        4.9 s
//!     memory pattern off           3.34 GB        4.6 s
//!     intra-op spinning off        3.37 GB        4.9 s
//!
//! Only prepacking moved the number: **−0.82 GB (−24%), and slightly FASTER**, so
//! there is no trade to weigh.
//!
//! The other three are documented here because each is the obvious next thing to
//! try and each is a dead end. Disabling the CPU arena is actively HARMFUL (+0.72
//! GB — without the pool, per-run buffers fragment the allocator instead of being
//! reused). Memory pattern is within noise. And spinning — the one that sounds
//! most like a CPU bug — burns **0.00 s of CPU over a 15 s idle window** either
//! way, because ORT's spin window is milliseconds and then the threads block. The
//! high CPU in that report was real inference during a re-scan, not idle spinning;
//! that is what the upload spool and the background nice level address.
//!
//! Do not add these back without re-measuring. That is what the harness in this
//! module's history was for, and it is why this table exists.
//!
//! [pf]: https://huggingface.co/openai/privacy-filter

use std::path::Path;

use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use crate::ner::{NerModel, NerToken};

/// Attention band — a token attends within ±this many tokens. From the
/// checkpoint's `sliding_window`, and the reason windowed inference is exact.
pub const BAND: usize = 128;
/// Tokens per forward pass. Measured against 512/1024/4096 on a 9k-token turn;
/// this was fastest. Not a correctness knob — any value ≥ 2·[`BAND`] is exact —
/// so it can be retuned on evidence without touching the output.
pub const WINDOW: usize = 2048;
/// Tokens two neighbouring windows share. Must be ≥ [`BAND`] or interior tokens
/// near a seam would see less context than the whole-input pass gives them, and
/// the exactness argument above collapses.
pub const OVERLAP: usize = 256;

const _: () = assert!(
    OVERLAP >= BAND,
    "overlap must cover the attention band, or windowing stops being exact"
);
const _: () = assert!(WINDOW > OVERLAP, "windows must advance");

/// Logit units subtracted from the background class before decoding — a thumb on
/// the scale toward *finding* a span.
///
/// **Zero, because it was measured.** The asymmetry argues for a positive bias:
/// over-redacting a word costs a word, missing one ships a name or an API key. But
/// on the real weights it changed nothing — every PII case in
/// `privacy_filter_real.rs` was caught at 0.0, 1.5 and 3.0 alike, and the one
/// technical term the model over-reads ("Cursor Bugbot" as a person) is a confident
/// call that no bias tips. A knob that only adds false positives, with no measured
/// recall to show for it, is worse than no knob — so we ship the checkpoint's own
/// calibrated operating point.
///
/// That measurement is ten strings, not a benchmark. `MODELSTAT_REDACTOR_RECALL_BIAS`
/// stays for a deployment that fears a miss more than noise, and the test prints the
/// trade-off at several points so the next person can re-decide with better data.
pub const RECALL_BIAS: f32 = 0.0;

/// The operating point in force, `MODELSTAT_REDACTOR_RECALL_BIAS` overriding
/// [`RECALL_BIAS`]. A knob rather than a constant because the right answer depends
/// on what a deployment fears more, and someone who wants the model's own
/// calibrated default should not have to fork us to get it.
///
/// Public because the span cache keys on it: an answer computed at one bias must
/// never be served at another, so the cache fingerprint has to name the SAME
/// value [`PrivacyFilter::load`] will actually use.
pub fn recall_bias() -> f32 {
    std::env::var("MODELSTAT_REDACTOR_RECALL_BIAS")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|b| b.is_finite() && *b >= 0.0)
        .unwrap_or(RECALL_BIAS)
}

/// The BIOES boundary tags. Only the class-count arithmetic reads them — the
/// labels themselves come from the checkpoint — so they live with that check.
#[cfg(test)]
const BOUNDARY_TAGS: [&str; 4] = ["B", "I", "E", "S"];

/// OpenAI Privacy Filter over ONNX Runtime, on CPU.
pub struct PrivacyFilter {
    session: std::sync::Mutex<Session>,
    tokenizer: Tokenizer,
    /// Label id → BIOES label, e.g. `"B-private_person"`. Read from the
    /// checkpoint rather than hardcoded, so a retrained head cannot silently
    /// shift what a class means.
    id2label: Vec<String>,
    /// Added to the background class's logit before decoding — a negative number
    /// derived from [`RECALL_BIAS`]. Lowering "stay background" and raising
    /// "start a span" are the same move, and this way it is one subtraction.
    background_penalty: f32,
    /// The id of the outside/background class (`"O"`).
    background_id: usize,
}

impl PrivacyFilter {
    /// Load from a model dir holding `config.json`, `tokenizer.json` and
    /// `onnx/model_q4.onnx` (+ its `.onnx_data` sidecar, which ONNX Runtime picks
    /// up by name from the same directory).
    pub fn load(model_dir: &Path) -> Result<Self, String> {
        Self::with_recall_bias(model_dir, recall_bias())
    }

    /// Load at an explicit operating point. Exists so the trade-off can be
    /// MEASURED rather than argued about — see the real-weights test, which runs
    /// the same inputs at several biases.
    pub fn with_recall_bias(model_dir: &Path, recall_bias: f32) -> Result<Self, String> {
        let cfg_bytes = std::fs::read(model_dir.join("config.json")).map_err(|e| e.to_string())?;
        let cfg: serde_json::Value =
            serde_json::from_slice(&cfg_bytes).map_err(|e| e.to_string())?;
        let id2label = parse_id2label(&cfg)?;
        let background_id = id2label
            .iter()
            .position(|l| l == "O")
            .ok_or("checkpoint has no background class")?;

        let tokenizer =
            Tokenizer::from_file(model_dir.join("tokenizer.json")).map_err(|e| e.to_string())?;

        let session = Session::builder()
            .map_err(|e| e.to_string())?
            // Bounded so the redactor cannot starve the machine it is protecting:
            // this runs in the background on someone's laptop while they work.
            .with_intra_threads(intra_threads())
            .map_err(|e| e.to_string())?
            // Don't PREPACK the weights — see the module docs for the numbers.
            //
            // Prepacking rewrites each int4 weight tensor into a layout the matmul
            // kernel prefers, and keeps BOTH copies for the life of the session.
            // On a 128-expert MoE that is a lot of tensors, and it is why an idle
            // daemon sat on gigabytes it was never going to use again. It buys
            // throughput for a server doing back-to-back inference on the same
            // weights; we redact in short bursts and then sleep for minutes, so we
            // are paying the memory and not collecting the speed.
            .with_prepacking(false)
            .map_err(|e| e.to_string())?
            .commit_from_file(model_dir.join("onnx").join("model_q4.onnx"))
            .map_err(|e| e.to_string())?;

        Ok(Self {
            session: std::sync::Mutex::new(session),
            tokenizer,
            id2label,
            background_penalty: recall_bias,
            background_id,
        })
    }

    /// Label one window: `[1, T]` ids in, one label id per token out.
    ///
    /// The forward pass runs inside [`tokio::task::block_in_place`] when a
    /// multi-thread runtime is present. Inference is SYNCHRONOUS CPU work —
    /// hundreds of milliseconds per window, minutes across a giant session — and
    /// `classify` is reached from async code (the flush path), so without this
    /// it pins a runtime worker for the whole redaction pass. On this machine
    /// that starved the daemon's status writer past the supervisor's 120s
    /// staleness bound, which KILLED the daemon mid-scan; the replacement
    /// rescanned, hit the same file, and died the same way — a crash-loop that
    /// kept the whole backlog stuck on one big session all day. `block_in_place`
    /// tells tokio to move other tasks off this worker first, so heartbeats keep
    /// writing while the model runs. Outside a multi-thread runtime (one-shot
    /// CLI commands, unit tests) it would panic, so fall back to running inline
    /// — blocking is harmless where nothing else shares the thread.
    fn label_window(&self, ids: &[i64]) -> Result<Vec<usize>, String> {
        let on_multithread_runtime = tokio::runtime::Handle::try_current()
            .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
            .unwrap_or(false);
        if on_multithread_runtime {
            tokio::task::block_in_place(|| self.label_window_blocking(ids))
        } else {
            self.label_window_blocking(ids)
        }
    }

    fn label_window_blocking(&self, ids: &[i64]) -> Result<Vec<usize>, String> {
        let shape = [1usize, ids.len()];
        let input = Tensor::from_array((shape, ids.to_vec())).map_err(|e| e.to_string())?;
        let mask = Tensor::from_array((shape, vec![1i64; ids.len()])).map_err(|e| e.to_string())?;
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let outputs = session
            .run(ort::inputs! { "input_ids" => input, "attention_mask" => mask })
            .map_err(|e| e.to_string())?;
        let (dims, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        let classes = *dims.last().ok_or("empty logits")? as usize;
        if classes != self.id2label.len() {
            return Err(format!(
                "checkpoint labels {} classes but the graph emits {classes}",
                self.id2label.len()
            ));
        }
        Ok(self.decode(data, classes))
    }

    /// Pick a label per token, biased toward finding spans.
    ///
    /// The model card describes a constrained Viterbi over BIOES; what actually
    /// matters for redaction is (a) the recall bias and (b) that impossible
    /// sequences do not invent boundaries. Since our consumer merges spans by type
    /// and then snaps them to whole words, an `I-` that follows nothing is
    /// harmless — it becomes its own span over the same characters. So this stays a
    /// biased argmax and the guard against silly output lives in the tests, rather
    /// than carrying a full trellis whose only visible effect would be which of two
    /// equally-redacted spellings of the same span we produce.
    fn decode(&self, logits: &[f32], classes: usize) -> Vec<usize> {
        logits
            .chunks_exact(classes)
            .map(|row| {
                let mut best = self.background_id;
                let mut best_score = row[self.background_id] - self.background_penalty;
                for (i, &score) in row.iter().enumerate() {
                    if i != self.background_id && score > best_score {
                        best = i;
                        best_score = score;
                    }
                }
                best
            })
            .collect()
    }
}

impl NerModel for PrivacyFilter {
    fn classify(&self, text: &str) -> Option<Vec<NerToken>> {
        self.try_classify(text)
            .inspect_err(|e| modelstat_log::log_warn!("privacy filter could not classify: {e}"))
            .ok()
    }
}

impl PrivacyFilter {
    fn try_classify(&self, text: &str) -> Result<Vec<NerToken>, String> {
        // Tokenized ONCE, so every token keeps a byte offset into the whole text no
        // matter which window ends up labelling it.
        let enc = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| e.to_string())?;
        let ids: Vec<i64> = enc.get_ids().iter().map(|&i| i64::from(i)).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let offsets = enc.get_offsets();
        let words = enc.get_tokens();

        let mut labels: Vec<usize> = Vec::with_capacity(ids.len());
        for (start, end) in plan_windows(ids.len(), WINDOW, OVERLAP) {
            let window = self.label_window(&ids[start..end])?;
            // Keep the earlier window's answer across the overlap: those tokens
            // were decoded with more left context.
            let fresh = labels.len().saturating_sub(start);
            labels.extend(window.into_iter().skip(fresh));
        }

        Ok(labels
            .into_iter()
            .enumerate()
            .map(|(i, id)| {
                let (sb, eb) = offsets.get(i).copied().unwrap_or((0, 0));
                NerToken {
                    entity: self.id2label[id].clone(),
                    word: words.get(i).cloned().unwrap_or_default(),
                    start: Some(crate::ner::byte_to_char(text, sb)),
                    end: Some(crate::ner::byte_to_char(text, eb)),
                }
            })
            .collect())
    }
}

/// The windows a `len`-token sequence is labelled in — `(start, end)` pairs that
/// cover every token, overlapping so no interior token loses context.
///
/// Pure and separately tested: a gap here is a silently unscrubbed stretch of a
/// turn, which is the exact bug that shipped once already.
pub fn plan_windows(len: usize, window: usize, overlap: usize) -> Vec<(usize, usize)> {
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

/// Threads for one forward pass. Capped: this is a background process on a machine
/// someone is using, and the measured gain past a handful of threads is small.
fn intra_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).clamp(1, 8))
        .unwrap_or(4)
}

/// `id2label` from the checkpoint config, as a dense id-indexed vec.
fn parse_id2label(cfg: &serde_json::Value) -> Result<Vec<String>, String> {
    let map = cfg
        .get("id2label")
        .and_then(|v| v.as_object())
        .ok_or("config.json has no id2label")?;
    let mut out = vec![String::new(); map.len()];
    for (k, v) in map {
        let i: usize = k.parse().map_err(|_| format!("bad label id {k}"))?;
        let label = v.as_str().ok_or("label is not a string")?;
        *out.get_mut(i)
            .ok_or_else(|| format!("label id {i} out of range"))? = label.to_string();
    }
    if out.iter().any(String::is_empty) {
        return Err("id2label has gaps".into());
    }
    Ok(out)
}

/// Every PII category this model names. Kept here because it is the vocabulary the
/// wire, the stored redaction counts and the analytics all key on — one list, so
/// they cannot drift apart.
pub const CATEGORIES: [&str; 8] = [
    "account_number",
    "private_address",
    "private_date",
    "private_email",
    "private_person",
    "private_phone",
    "private_url",
    "secret",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_lands_in_a_window() {
        for len in [0usize, 1, 5, WINDOW - 1, WINDOW, WINDOW + 1, 9_013, 65_537] {
            let plan = plan_windows(len, WINDOW, OVERLAP);
            if len == 0 {
                assert!(plan.is_empty());
                continue;
            }
            let mut covered = vec![false; len];
            for &(a, b) in &plan {
                assert!(b > a && b <= len, "bad window {a}..{b} for len {len}");
                assert!(b - a <= WINDOW, "window {a}..{b} is wider than one pass");
                for c in covered[a..b].iter_mut() {
                    *c = true;
                }
            }
            assert!(
                covered.iter().all(|c| *c),
                "len {len} left tokens unclassified: {plan:?}"
            );
            for pair in plan.windows(2) {
                let (prev, next) = (pair[0], pair[1]);
                assert!(
                    prev.1 - next.0 >= BAND,
                    "windows {prev:?}/{next:?} share less than the attention band, \
                     so windowing is no longer exact"
                );
            }
        }
    }

    #[test]
    fn a_pathological_overlap_still_makes_progress() {
        let plan = plan_windows(2_000, 100, 100);
        assert!(plan.len() < 3_000, "a zero stride would never terminate");
        assert_eq!(plan.last().unwrap().1, 2_000);
    }

    #[test]
    fn the_categories_match_the_boundary_tag_arithmetic() {
        // 1 background + 8 categories x 4 BIOES tags = the checkpoint's 33 classes.
        assert_eq!(1 + CATEGORIES.len() * BOUNDARY_TAGS.len(), 33);
    }

    #[test]
    fn id2label_is_parsed_densely_and_rejects_gaps() {
        let good = serde_json::json!({"id2label": {"0": "O", "1": "S-secret"}});
        assert_eq!(parse_id2label(&good).unwrap(), vec!["O", "S-secret"]);
        let gappy = serde_json::json!({"id2label": {"0": "O", "2": "S-secret"}});
        assert!(
            parse_id2label(&gappy).is_err(),
            "a gap must not read as label 1"
        );
        assert!(parse_id2label(&serde_json::json!({})).is_err());
    }

    #[test]
    fn the_bias_knob_only_ever_finds_more_never_less() {
        // Whatever the operating point, raising it must not make the decoder MISS
        // something it already caught. Checked as arithmetic because the shipped
        // value is 0.0 (measured — see the const's docs) and a future edit could
        // set it hot without re-reading why it exists.
        let decide = |bias: f32, background_margin: f32| -> usize {
            let row = [background_margin, 0.0f32];
            let mut best = 0usize;
            let mut best_score = row[0] - bias;
            for (i, &s) in row.iter().enumerate() {
                if i != 0 && s > best_score {
                    best = i;
                    best_score = s;
                }
            }
            best
        };
        // A clear background stays background at the shipped point…
        assert_eq!(decide(RECALL_BIAS, 1.0), 0);
        // …and a hotter point can only flip cases TOWARD redaction.
        for margin in [0.1f32, 0.5, 1.0, 2.0] {
            let cool = decide(0.0, margin);
            let hot = decide(3.0, margin);
            assert!(
                hot >= cool,
                "raising the bias must never un-redact something (margin {margin})"
            );
        }
        assert_eq!(decide(3.0, 0.5), 1, "a hot point redacts a marginal call");
    }
}
