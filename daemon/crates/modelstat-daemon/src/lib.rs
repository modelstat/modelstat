//! Collector runtime: boot, lock, supervise, heartbeat, watcher, scan orchestration, reconcile, quiesce, shutdown.
//!
//! Implemented in milestone M1/M4 (see core/specs/daemon/plan.md §5). M0
//! stands the crate up so the workspace builds on all six targets and the
//! dependency graph (esp. the no-llama-link boundary) is enforced now.
