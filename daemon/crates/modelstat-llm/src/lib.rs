//! llama.cpp runtime (model download, lazy load / idle unload, GPU guard, serialized queue). LINKED ONLY BY modelstat-summarizer.
//!
//! Implemented in milestone M3 (see core/specs/daemon/plan.md §5). M0
//! stands the crate up so the workspace builds on all six targets and the
//! dependency graph (esp. the no-llama-link boundary) is enforced now.
