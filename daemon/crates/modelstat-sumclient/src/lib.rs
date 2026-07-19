//! `modelstat-sumclient` — the summarizer protocol-v1 client (feature §9, §10.4).
//!
//! A minimal, deliberately non-OpenAI-compatible client: `GET /healthz` +
//! `POST /v1/complete`, with the §9 retry matrix (≤3 attempts, `Retry-After`,
//! status-only errors) and a one-time protocol/version skew warning. The
//! resilient hold-and-retry wrapper (never a degraded fallback, §9.4) lives in
//! the pipeline; this crate is the protocol layer, shared by the collector
//! client and the engine's server (`modelstat-summarizer`).
//!
//! This crate has NO llama.cpp dependency (plan D4 boundary): it is pure
//! reqwest/serde, so the collector links it freely.

pub mod client;
pub mod protocol;

pub use client::{SumError, SummarizerClient};
pub use protocol::{
    CompleteRequest, CompleteResponse, EngineError, HealthResponse, MODEL_ID, PROTOCOL_VERSION,
};
