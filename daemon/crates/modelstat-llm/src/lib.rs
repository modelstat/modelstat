//! `modelstat-llm` — the engine's inference runtime (feature §10). LINKED ONLY BY
//! `modelstat-summarizer` (plan D4 / CI no-llama-link).
//!
//! This crate owns everything AROUND the raw model: the [`Engine`] lifecycle
//! (lazy load, single serialized worker, idle-unload, download-on-first-use,
//! drain-on-shutdown), the [`Backend`] abstraction, `summarizer.json`
//! ([`EngineConfig`]), the GPU-abort [`guard`], and `<think>`-stripping.
//!
//! The raw model runtime is a [`Backend`]. The native llama.cpp backend (behind
//! the `llama` feature — needs cmake) slots into the trait; [`UnavailableBackend`]
//! is the fail-loud stand-in for cmake-free builds, and [`MockBackend`] (test /
//! `mock` feature) exercises the lifecycle. No stub ever fabricates a completion.

pub mod backend;
pub mod config;
pub mod engine;
pub mod guard;

#[cfg(feature = "llama")]
pub mod llama;

pub use backend::{strip_think, Backend, GenParams, UnavailableBackend};
pub use config::EngineConfig;
pub use engine::{CompleteOutcome, Engine, EngineState};

#[cfg(feature = "llama")]
pub use llama::LlamaBackend;

#[cfg(any(test, feature = "mock"))]
pub use backend::MockBackend;
