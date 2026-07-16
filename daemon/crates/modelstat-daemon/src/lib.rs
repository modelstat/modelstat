//! Collector runtime: boot, lock, supervise, heartbeat, watcher, scan orchestration, reconcile, quiesce, shutdown.
//!
//! Ported piece-by-piece in M4 Part 3 (see core/specs/daemon/plan.md §5). The
//! self-contained runtime primitives land first (each green + tested); the scan
//! orchestration + main loop compose them.

pub mod adapters;
pub mod authoritative_git;
pub mod discover_jobs;
pub mod enrich_scripts;
pub mod flush;
pub mod insights;
pub mod lock;
pub mod processing_version;
pub mod reconcile;
pub mod runtime;
pub mod scan;
pub mod single_flight;
pub mod watch;

pub use authoritative_git::resolve_authoritative_git;
pub use discover_jobs::{
    discover_jobs, order_jobs_newest_first, parse_job, ParserKind, ScanJob,
};
pub use flush::{build_flush_batches, with_non_null_tokens, FlushOutcome, PreparedBatch};
pub use reconcile::{
    reconcile_backfill, BackfillDays, BackfillDaySessions, ReconcileOutcome, ReconcileStore,
    BackfillDigest,
};
pub use scan::{
    run_scan_over_jobs, BatchUploader, CursorStore, Hold, RunScanOptions, ScanObserver,
    ScanTallies, BATCH_MAX_EVENTS, BATCH_MAX_TOOL_CALLS, MAX_FILES_PER_SCAN,
};

pub use lock::{
    acquire_daemon_lock, check_lock_ownership, daemon_lock_path, is_process_alive,
    read_daemon_lock, remove_lock_if_owned, AcquireOpts, AcquireResult, LockMeta, OwnershipCheck,
};
pub use processing_version::{
    reconcile_processing_version, ProcessingState, VersionReconcile, PROCESSING_VERSION,
};
pub use single_flight::CoalescingRunner;
