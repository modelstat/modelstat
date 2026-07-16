//! Collector runtime: boot, lock, supervise, heartbeat, watcher, scan orchestration, reconcile, quiesce, shutdown.
//!
//! Ported piece-by-piece in M4 Part 3 (see core/specs/daemon/plan.md §5). The
//! self-contained runtime primitives land first (each green + tested); the scan
//! orchestration + main loop compose them.

pub mod lock;
pub mod processing_version;
pub mod single_flight;

pub use lock::{
    acquire_daemon_lock, check_lock_ownership, daemon_lock_path, is_process_alive,
    read_daemon_lock, remove_lock_if_owned, AcquireOpts, AcquireResult, LockMeta, OwnershipCheck,
};
pub use processing_version::{
    reconcile_processing_version, ProcessingState, VersionReconcile, PROCESSING_VERSION,
};
pub use single_flight::CoalescingRunner;
