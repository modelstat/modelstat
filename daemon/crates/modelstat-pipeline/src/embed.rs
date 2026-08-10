//! The embedder seam (feature §9.5, plan D5) — BGE-small-en-v1.5, 384-dim,
//! mean-pooling + L2, CPU. The candle model runtime slots in behind [`Embedder`];
//! the pure post-processing (mean-pool, L2-normalize) + the segmentation glue are
//! here so they are tested without downloading a model.
//!
//! Embedding is best-effort: a failure yields an EMPTY vector and segmentation
//! falls back to the time-gap heuristic (§9.5 / segment.rs) — the scan never dies
//! because a model is missing.

use modelstat_wire::RawEvent;

use crate::segment::{turn_meta, turn_surface, TurnMeta};

/// The wire embedding dimension (BGE-small). Any backend must produce this or an
/// empty vector.
pub const EMBED_DIM: usize = 384;

/// Turns a redaction-safe metadata surface into a 384-dim vector. Failures return
/// an empty vector (segmentation degrades to time-gap, never crashes).
pub trait Embedder {
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// The fail-open default: no embeddings. Segmentation falls back to time/turn/
/// content heuristics. Used until the candle BGE model is present.
pub struct NoEmbedder;

impl Embedder for NoEmbedder {
    fn embed(&self, _text: &str) -> Vec<f32> {
        Vec::new()
    }
}

/// Mean-pool token embeddings weighted by the attention mask (the sentence-
/// transformer pooling BGE uses). `token_embeddings[i]` is token `i`'s hidden
/// vector; `mask[i]` is 1.0 for a real token, 0.0 for padding.
pub fn mean_pool(token_embeddings: &[Vec<f32>], mask: &[f32]) -> Vec<f32> {
    if token_embeddings.is_empty() {
        return Vec::new();
    }
    let dim = token_embeddings[0].len();
    let mut sum = vec![0.0f32; dim];
    let mut total = 0.0f32;
    for (tok, &m) in token_embeddings.iter().zip(mask.iter()) {
        if tok.len() != dim {
            continue;
        }
        for (s, &v) in sum.iter_mut().zip(tok.iter()) {
            *s += v * m;
        }
        total += m;
    }
    if total > 0.0 {
        for s in &mut sum {
            *s /= total;
        }
    }
    sum
}

/// L2-normalize in place (unit length). A zero vector is left as-is (its cosine
/// distance is defined as 1 by `segment::cosine_distance`).
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Embed every event's metadata surface (§18) into a [`TurnMeta`] for
/// segmentation. A per-turn embedding failure just leaves that turn's embedding
/// empty (the cosine check is skipped for its pairs).
pub fn embed_turns<E: Embedder>(events: &[RawEvent], embedder: &E) -> Vec<TurnMeta> {
    events
        .iter()
        .map(|e| {
            let surface = turn_surface(e);
            let embedding = if surface.is_empty() {
                Vec::new()
            } else {
                embedder.embed(&surface)
            };
            turn_meta(e, embedding)
        })
        .collect()
}

// ── candle BGE runtime (feature `candle`) ────────────────────────────────────
//
// Compile-verified against candle-transformers 0.8; run-verification needs the
// downloaded BGE weights (exact vs transformers.js parity is not required —
// PROCESSING_VERSION 16 absorbs the runtime swap, plan R2/D13).
#[cfg(feature = "candle")]
mod candle_bge {
    use std::path::Path;

    use candle_core::{Device, Tensor};
    use candle_nn::VarBuilder;
    use candle_transformers::models::bert::{BertModel, Config, DTYPE};
    use tokenizers::Tokenizer;

    use super::{l2_normalize, mean_pool, Embedder};

    /// BAAI/bge-small-en-v1.5 over candle (CPU, 384-dim). Loaded from a model dir
    /// holding `config.json`, `tokenizer.json`, `model.safetensors`.
    pub struct CandleEmbedder {
        model: BertModel,
        tokenizer: Tokenizer,
        device: Device,
    }

    impl CandleEmbedder {
        /// Load the model from `model_dir`. Errs (String) on any missing/invalid
        /// artifact — the caller keeps the fail-open [`super::NoEmbedder`] then.
        pub fn load(model_dir: &Path) -> Result<Self, String> {
            let device = Device::Cpu;
            let cfg_bytes =
                std::fs::read(model_dir.join("config.json")).map_err(|e| e.to_string())?;
            let config: Config = serde_json::from_slice(&cfg_bytes).map_err(|e| e.to_string())?;
            let tokenizer = Tokenizer::from_file(model_dir.join("tokenizer.json"))
                .map_err(|e| e.to_string())?;
            let weights = model_dir.join("model.safetensors");
            // SAFETY: mmap of a trusted, checksum-verified model file we downloaded.
            let vb = unsafe {
                VarBuilder::from_mmaped_safetensors(&[weights], DTYPE, &device)
                    .map_err(|e| e.to_string())?
            };
            let model = BertModel::load(vb, &config).map_err(|e| e.to_string())?;
            Ok(Self {
                model,
                tokenizer,
                device,
            })
        }

        fn try_embed(&self, text: &str) -> Result<Vec<f32>, String> {
            let enc = self
                .tokenizer
                .encode(text, true)
                .map_err(|e| e.to_string())?;
            let ids: Vec<u32> = enc.get_ids().to_vec();
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let input_ids = Tensor::new(ids.as_slice(), &self.device)
                .and_then(|t| t.unsqueeze(0))
                .map_err(|e| e.to_string())?; // [1, seq]
            let token_type_ids = input_ids.zeros_like().map_err(|e| e.to_string())?;
            // Single sentence, no padding → every token is attended (mask all 1s).
            let hidden = self
                .model
                .forward(&input_ids, &token_type_ids, None)
                .and_then(|h| h.squeeze(0)) // [seq, hidden]
                .map_err(|e| e.to_string())?;
            let tokens: Vec<Vec<f32>> = hidden.to_vec2::<f32>().map_err(|e| e.to_string())?;
            let mask = vec![1.0f32; tokens.len()];
            let mut pooled = mean_pool(&tokens, &mask);
            l2_normalize(&mut pooled);
            Ok(pooled)
        }
    }

    impl Embedder for CandleEmbedder {
        fn embed(&self, text: &str) -> Vec<f32> {
            // Best-effort: any inference failure → empty vector (§9.5 — segmentation
            // falls back to time-gap, the scan never dies).
            self.try_embed(text).unwrap_or_default()
        }
    }
}

#[cfg(feature = "candle")]
pub use candle_bge::CandleEmbedder;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_pool_ignores_padding() {
        let tokens = vec![vec![2.0, 0.0], vec![0.0, 4.0], vec![9.0, 9.0]];
        let mask = vec![1.0, 1.0, 0.0]; // third is padding
        assert_eq!(mean_pool(&tokens, &mask), vec![1.0, 2.0]);
    }

    #[test]
    fn l2_normalize_unit_length() {
        let mut v = vec![3.0f32, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn no_embedder_yields_time_gap_segmentation() {
        use crate::segment::segment_turns;
        use modelstat_wire::RawEvent;
        let mk = |ts: &str| RawEvent {
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            source_event_id: "e".into(),
            ts: ts.into(),
            kind: "user_message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s".into(),
            actor_id: None,
            recipient_actor_id: None,
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: None,
            tokens_unmapped: std::collections::BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: vec![],
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        };
        let events = vec![
            mk("2026-06-01T10:00:00.000Z"),
            mk("2026-06-01T10:01:00.000Z"),
            mk("2026-06-01T10:30:00.000Z"), // +29 min → time-gap split
        ];
        let turns = embed_turns(&events, &NoEmbedder);
        assert!(turns.iter().all(|t| t.embedding.is_empty()));
        let segs = segment_turns(&turns);
        // The 29-min gap splits; the trailing singleton merges back.
        assert_eq!(segs, vec![vec![0, 1, 2]]);
    }
}
