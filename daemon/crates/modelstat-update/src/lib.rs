//! Self-update: release check / download / verify / swap-both / rollback.
//!
//! Implemented in milestone M5/M6 (see core/specs/daemon/plan.md §5). M0
//! stands the crate up so the workspace builds on all six targets and the
//! dependency graph (esp. the no-llama-link boundary) is enforced now.
