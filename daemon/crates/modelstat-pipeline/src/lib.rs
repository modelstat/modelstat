//! `modelstat-pipeline` — collector-side summarization (feature §9–§11, §18).
//!
//! Landed so far: the six frozen system prompts + their constants ([`prompts`],
//! the §18 contract, verbatim from the TS), and the resilient hold-and-retry
//! summarizer ([`resilient`] — the no-silent-degradation core, §9.4). The
//! the PII detector + BGE embedder, segmentation, and the six passes slot in over the
//! same protocol client + trait seams. No llama.cpp (plan D4): summarization runs
//! over the summarizer protocol (`modelstat-sumclient`).

pub mod batch;
pub mod build;
pub mod embed;
pub mod passes;
pub mod prompts;
pub mod resilient;
pub mod segment;
pub mod session_metadata;

pub use batch::{
    attach_segment_ids, attach_segment_ids_by_map, batch_id, deep_redact_tool_commands,
    enrich_tool_call_redaction, prepare_cloud_raw_events, ulid,
};
pub use build::{build_for_one_session, build_session_titles, BuildOutcome};
pub use embed::{embed_turns, l2_normalize, mean_pool, Embedder, NoEmbedder, EMBED_DIM};
pub use passes::{CognitionTags, THINKING_HEADROOM_TOKENS};
pub use resilient::{
    preflight, PreflightReport, ResilientSummarizer, SummarizeOutcome, Summarizer, DEFAULT_COOLDOWN,
};
pub use segment::{
    cosine_distance, install_calibration, installed_calibration, segment_turns, segment_turns_with,
    turn_meta, turn_surface, Calibration, TurnMeta, CALIBRATION_CONFIG_KIND,
};
pub use session_metadata::{build_session_metadata, LinkExtractor};
