//! Collector runtime: boot, lock, supervise, heartbeat, watcher, scan orchestration, reconcile, quiesce, shutdown.
//!
//! Ported piece-by-piece in M4 Part 3 (see core/specs/daemon/plan.md §5). The
//! self-contained runtime primitives land first (each green + tested); the scan
//! orchestration + main loop compose them.

pub mod processing_version;
pub mod single_flight;

pub use processing_version::{
    reconcile_processing_version, ProcessingState, VersionReconcile, PROCESSING_VERSION,
};
pub use single_flight::CoalescingRunner;
