//! `modelstat-pipeline` — collector-side summarization (feature §9–§11, §18).
//!
//! Landed so far: the six frozen system prompts + their constants ([`prompts`],
//! the §18 contract, verbatim from the TS), and the resilient hold-and-retry
//! summarizer ([`resilient`] — the no-silent-degradation core, §9.4). The
//! candle NER + BGE embedder, segmentation, and the six passes slot in over the
//! same protocol client + trait seams. No llama.cpp (plan D4): summarization runs
//! over the summarizer protocol (`modelstat-sumclient`).

pub mod prompts;
pub mod resilient;

pub use resilient::{ResilientSummarizer, SummarizeOutcome, Summarizer, DEFAULT_COOLDOWN};
