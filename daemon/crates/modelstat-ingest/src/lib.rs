//! IngestClient (retry matrix, byte clamps), device API client, recoverIdentity, backfill digests.
//!
//! Implemented in milestone M1/M4 (see core/specs/daemon/plan.md §5). M0
//! stands the crate up so the workspace builds on all six targets and the
//! dependency graph (esp. the no-llama-link boundary) is enforced now.
