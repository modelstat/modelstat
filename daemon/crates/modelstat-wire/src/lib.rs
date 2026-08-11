//! `modelstat-wire` — the single source of truth for the modelstat wire
//! contract on the Rust side: deterministic ids, machine key + device UUID,
//! canonical enums, serde schemas, frozen byte caps, and the UTF-8 clamp layer.
//!
//! Everything here is a byte-for-byte port of the TypeScript
//! (`@modelstat/core` + `@modelstat/parsers` id/hash helpers) that must stay
//! identical so existing devices, SDKs, the tray, and the server notice nothing
//! but a version bump (plan D12/D16). The golden vectors under `tests/golden/`
//! are generated from the TS and pin every value; the TS↔Rust parity test
//! (`packages/core/src/wire-parity.test.ts`) closes the loop from the other side.

pub mod caps;
pub mod clamp;
pub mod device;
pub mod enums;
pub mod ids;
pub mod param_shape;
pub mod schema;

// Flat re-exports for the common surface.
pub use clamp::clamp_utf8_bytes;
pub use device::{
    device_uuid_from_machine_key, intended_device_uuid, machine_key_hash, DEVICE_UUID_NAMESPACE,
    MACHINE_KEY_SALT,
};
pub use ids::{segment_id, source_event_id, tc_fallback_id, EventSource};
pub use param_shape::param_shape;
pub use schema::{
    AnchorPr, Fingerprint, GitContext, HeartbeatPayload, IngestBatch, RawEvent, RedactionReport,
    RegisterRequest, RepoAnchors, ScriptSummary, Segment, SegmentBehavior, SegmentGeneration,
    SegmentLocalTime, TaxonomyHintRooted, TokenUsage, ToolAction, ToolCallWire,
    SLUG_SOURCE_GIT_REMOTE, SLUG_SOURCE_PATH_SHAPE, SLUG_SOURCE_REPO_ROOT_DIR,
};
