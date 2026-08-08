//! `modelstat-redact` — the compiled-in redaction floor.
//!
//! A byte-for-byte port of the wire floor (`packages/core/src/redact.ts` +
//! `redact-floor.ts`): 18 ordered secret patterns, an additive policy bundle
//! (`policies.ts`), the entropy pass, email + absolute-path redaction (with the
//! Windows shapes feature §17.4 mandates), and repo-root relativization.
//! Fail-closed and never remotely weakenable (feature §21.6).
//!
//! Layer-2 (the PII detector) and layer-3 (local-only LLM backstop) redaction land in
//! M3 (feature §9.5); this crate is layer 1, the irreducible baseline.

mod entropy;
mod floor;
mod paths;
pub mod pii;
pub mod policy;
#[cfg(feature = "onnx")]
pub mod privacy_filter;
mod redact;
pub mod remote;
#[cfg(feature = "cache")]
pub mod span_cache;

pub use floor::FLOOR_REPLACEMENT_TEMPLATES;
pub use pii::{
    pii_redact, pii_redact_checked, pii_redact_checked_many, redactor_active, PiiModel,
    PiiRedaction, PiiToken, UnavailableRedactor,
};
pub use policy::{
    compile_policy_patterns, install_policy_patterns, installed_policy_patterns, CompiledPattern,
    RedactionPattern, RedactionPolicyBundle, POLICIES_BUNDLED_FALLBACK, POLICIES_CONFIG_KIND,
};
pub use redact::{redact, redact_with_remote, RedactionCounts, RedactionResult};
#[cfg(feature = "cache")]
pub use span_cache::{CachedNer, SpanStore};

/// Serialize the tests that swap the process-wide policy augment
/// ([`policy::install_policy_patterns`]). Poison-safe: a panicking test releases
/// the lock instead of wedging the suite.
#[cfg(test)]
pub(crate) fn test_policy_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
