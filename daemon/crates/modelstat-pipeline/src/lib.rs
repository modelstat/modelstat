//! `modelstat-pipeline` — collector-side summarization (feature §9–§11, §18).
//!
//! Landed so far: the six frozen system prompts + their constants ([`prompts`],
//! the §18 contract, verbatim from the TS), and the resilient hold-and-retry
//! summarizer ([`resilient`] — the no-silent-degradation core, §9.4). The
//! candle NER + BGE embedder, segmentation, and the six passes slot in over the
//! same protocol client + trait seams. No llama.cpp (plan D4): summarization runs
//! over the summarizer protocol (`modelstat-sumclient`).

pub mod build;
pub mod embed;
pub mod passes;
pub mod prompts;
pub mod resilient;
pub mod segment;

pub use build::{build_for_one_session, build_session_titles, BuildOutcome};
pub use embed::{embed_turns, l2_normalize, mean_pool, Embedder, NoEmbedder, EMBED_DIM};
pub use passes::{CognitionTags, THINKING_HEADROOM_TOKENS};
pub use resilient::{
    preflight, PreflightReport, ResilientSummarizer, SummarizeOutcome, Summarizer, DEFAULT_COOLDOWN,
};
pub use segment::{cosine_distance, segment_turns, turn_meta, turn_surface, TurnMeta};
