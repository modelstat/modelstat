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
            source_event_id: "e".into(),
            ts: ts.into(),
            kind: "user_message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s".into(),
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: None,
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: vec![],
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            pricing_mode: None,
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
