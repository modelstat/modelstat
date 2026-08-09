//! Collector runtime: boot, lock, supervise, heartbeat, watcher, scan orchestration, reconcile, quiesce, shutdown.
//!
//! Ported piece-by-piece in M4 Part 3 (see core/specs/daemon/plan.md §5). The
//! self-contained runtime primitives land first (each green + tested); the scan
//! orchestration + main loop compose them.

pub mod adapters;
pub mod anchors;
pub mod authoritative_git;
pub mod claude_settings;
pub mod discover_jobs;
pub mod engine;
pub mod enrich_scripts;
pub mod flush;
pub mod insights;
pub mod lock;
pub mod priority;
pub mod processing_version;
pub mod reconcile;
pub mod rotate;
pub mod run;
pub mod runtime;
pub mod scan;
pub mod single_flight;
pub mod spool;
pub mod status;
pub mod statusline;
pub mod supervise;
pub mod uploader;
pub mod watch;

pub use authoritative_git::resolve_authoritative_git;
pub use discover_jobs::{discover_jobs, order_jobs_newest_first, parse_job, ParserKind, ScanJob};
pub use flush::{build_flush_batches, with_non_null_tokens, FlushOutcome, PreparedBatch};
pub use reconcile::{
    reconcile_backfill, BackfillDaySessions, BackfillDays, BackfillDigest, ReconcileOutcome,
    ReconcileStore,
};
pub use scan::{
    run_scan_over_jobs, BatchSink, CursorStore, Hold, RunScanOptions, ScanObserver, ScanTallies,
    BATCH_MAX_EVENTS, BATCH_MAX_TOOL_CALLS, MAX_FILES_PER_SCAN,
};
pub use spool::{Spool, SpoolDepth, SpoolEntry, SpoolError, SpooledBatch};
pub use uploader::{drain_once, run_drain_loop, BatchUploader, DrainOutcome, UploadObserver};

pub use lock::{
    acquire_daemon_lock, check_lock_ownership, daemon_lock_path, is_process_alive,
    read_daemon_lock, remove_lock_if_owned, AcquireOpts, AcquireResult, LockMeta, OwnershipCheck,
};
pub use processing_version::{
    reconcile_processing_aspects, ProcessingState, VersionReconcile, ASPECT_VERSIONS,
    LEGACY_WORLD_VERSION,
};
pub use single_flight::CoalescingRunner;

/// Test-only redactor fakes, shared by the scan/flush/adapter test modules.
///
/// They exist because a redactor that cannot answer now HOLDS in every mode — an
/// uploaded abstract is egress too — so a test that wants a working pipeline has
/// to hand it a working redactor, exactly like production does.
#[cfg(test)]
pub(crate) mod testing {
    use modelstat_redact::{PiiModel, PiiToken};

    /// Answers for any text, and redacts the liveness sentinel's name so
    /// `redactor_active` reads the layer as UP.
    pub struct AnsweringRedactor;

    impl PiiModel for AnsweringRedactor {
        fn classify(&self, text: &str) -> Option<Vec<PiiToken>> {
            let mut out = Vec::new();
            if let Some(i) = text.find("Katherine Johnson") {
                let tok = |entity: &str, word: &str, a: usize, b: usize| PiiToken {
                    entity: entity.into(),
                    word: word.into(),
                    start: Some(a),
                    end: Some(b),
                };
                out.push(tok("B-PER", "Katherine", i, i + 9));
                out.push(tok("I-PER", "Johnson", i + 10, i + 17));
            }
            Some(out)
        }
    }
}
