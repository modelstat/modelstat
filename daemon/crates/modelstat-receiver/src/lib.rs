//! Loopback receiver (SDK contract) + FileQueueStore + control plane.
//!
//! Implemented in milestone M4 (see core/specs/daemon/plan.md §5). M0
//! stands the crate up so the workspace builds on all six targets and the
//! dependency graph (esp. the no-llama-link boundary) is enforced now.

pub mod build_batches;
pub mod queue;

pub use build_batches::{
    build_batches, BuildBatchesOpts, DrainBatches, PipelineRunner, FORCE_SHIP_THRESHOLD,
    INGEST_BATCH_MAX_EVENTS, INGEST_BATCH_MAX_TOOL_CALLS, SESSION_DEBOUNCE_MS,
};
pub use queue::{FileQueueStore, QueueItem, QueueStore};
