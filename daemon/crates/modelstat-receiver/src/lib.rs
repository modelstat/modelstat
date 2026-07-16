//! Loopback receiver (SDK contract) + FileQueueStore + control plane.
//!
//! Implemented in milestone M4 (see core/specs/daemon/plan.md §5). M0
//! stands the crate up so the workspace builds on all six targets and the
//! dependency graph (esp. the no-llama-link boundary) is enforced now.

pub mod queue;

pub use queue::{FileQueueStore, QueueItem, QueueStore};
