//! `modelstat-redact` — the compiled-in redaction floor.
//!
//! A byte-for-byte port of the wire floor (`packages/core/src/redact.ts` +
//! `redact-floor.ts`): 18 ordered secret patterns, an additive signed policy
//! bundle (`policies.ts`), the entropy pass, email + absolute-path redaction
//! (with the Windows shapes feature §17.4 mandates), and repo-root
//! relativization. Fail-closed and never remotely weakenable (feature §21.6).
//!
//! Layer-2 (candle NER) and layer-3 (local-only LLM backstop) redaction land in
//! M3 (feature §9.5); this crate is layer 1, the irreducible baseline.

mod entropy;
mod floor;
mod paths;
pub mod policy;
mod redact;

pub use floor::FLOOR_REPLACEMENT_TEMPLATES;
pub use policy::{
    compile_policy_patterns, CompiledPattern, RedactionPattern, RedactionPolicyBundle,
    POLICIES_BUNDLED_FALLBACK, POLICIES_CONFIG_KIND,
};
pub use redact::{redact, redact_with_remote, RedactionCounts, RedactionResult};
