//! Local processing-pipeline version — a port of
//! `apps/daemon/src/processing-version.ts`.
//!
//! The marker that lets a new daemon build force a re-scan of every
//! previously-uploaded session. File cursors track "uploaded up to byte N", so a
//! normal restart only ships new events — but when the pipeline ITSELF changes
//! shape (summariser model/prompt, sampling, redaction, segment boundaries), every
//! previously-uploaded segment is stale even though the JSONL hasn't moved. On
//! startup the daemon compares this compiled-in integer to the one stored in
//! `state.json`; if higher, it wipes every cursor so the next scan re-reads the
//! world through the current pipeline (a re-scan REPLACES segments by
//! `segment_id` in place — no duplicates, no orphans).

use modelstat_ingest::RuntimeState;

/// Current local processing-pipeline version. The Rust rewrite ships **16** — the
/// cutover value that absorbs the runtime/model swaps the TS never had (the candle
/// BGE embedder + BERT-NER, and the prompt-fed non-determinism of a different
/// engine) in one bump, so every historical session re-scans once at cutover.
/// (The TS chain ended at v15; see that file for the v1–v15 history.) Bump when
/// the pipeline produces materially different segments for the same input.
pub const PROCESSING_VERSION: i64 = 16;

/// The state a reconcile reads + mutates: the stored marker plus the cursors it
/// wipes on a bump. Abstracted so the decision is unit-testable without touching
/// `state.json`.
pub trait ProcessingState {
    fn processing_version(&self) -> Option<i64>;
    fn set_processing_version(&mut self, v: i64);
    fn wipe_cursors(&mut self);
}

impl ProcessingState for RuntimeState {
    fn processing_version(&self) -> Option<i64> {
        self.processing_version
    }
    fn set_processing_version(&mut self, v: i64) {
        self.processing_version = Some(v);
    }
    fn wipe_cursors(&mut self) {
        self.cursor.clear();
    }
}

/// What a reconcile did — surfaced in the startup log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionReconcile {
    pub changed: bool,
    pub from: i64,
    pub to: i64,
}

/// On startup: if the stored version is older than the compiled-in one (or absent
/// → treated as v1), wipe cursors + stamp the new version so the next scan
/// re-reads the world through the current pipeline. Port of
/// `reconcileProcessingVersion`. The caller persists the mutated state (and logs
/// the outcome) only when `changed`.
pub fn reconcile_processing_version<S: ProcessingState>(state: &mut S) -> VersionReconcile {
    let stored = state.processing_version().unwrap_or(1);
    if stored >= PROCESSING_VERSION {
        return VersionReconcile {
            changed: false,
            from: stored,
            to: PROCESSING_VERSION,
        };
    }
    state.wipe_cursors();
    state.set_processing_version(PROCESSING_VERSION);
    VersionReconcile {
        changed: true,
        from: stored,
        to: PROCESSING_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeState {
        version: Option<i64>,
        cursors: usize,
        wiped: bool,
    }
    impl ProcessingState for FakeState {
        fn processing_version(&self) -> Option<i64> {
            self.version
        }
        fn set_processing_version(&mut self, v: i64) {
            self.version = Some(v);
        }
        fn wipe_cursors(&mut self) {
            self.cursors = 0;
            self.wiped = true;
        }
    }

    #[test]
    fn absent_version_wipes_and_stamps() {
        let mut s = FakeState {
            version: None,
            cursors: 7,
            wiped: false,
        };
        let r = reconcile_processing_version(&mut s);
        assert!(r.changed);
        assert_eq!(r.from, 1); // None → treated as v1
        assert_eq!(r.to, PROCESSING_VERSION);
        assert!(s.wiped);
        assert_eq!(s.cursors, 0);
        assert_eq!(s.version, Some(PROCESSING_VERSION));
    }

    #[test]
    fn older_version_wipes() {
        let mut s = FakeState {
            version: Some(9),
            cursors: 3,
            wiped: false,
        };
        let r = reconcile_processing_version(&mut s);
        assert!(r.changed);
        assert_eq!(r.from, 9);
        assert!(s.wiped);
        assert_eq!(s.version, Some(16));
    }

    #[test]
    fn current_or_newer_is_a_noop() {
        for v in [PROCESSING_VERSION, PROCESSING_VERSION + 1] {
            let mut s = FakeState {
                version: Some(v),
                cursors: 3,
                wiped: false,
            };
            let r = reconcile_processing_version(&mut s);
            assert!(!r.changed);
            assert!(!s.wiped);
            assert_eq!(s.cursors, 3); // cursors untouched
            assert_eq!(s.version, Some(v)); // version untouched
        }
    }

    #[test]
    fn reconciles_a_real_runtime_state() {
        // The bridge to the M1 state store: a default (version-absent) state
        // reconciles up to the current version.
        let mut s = RuntimeState::default();
        assert_eq!(s.processing_version, None);
        let r = reconcile_processing_version(&mut s);
        assert!(r.changed);
        assert_eq!(s.processing_version, Some(PROCESSING_VERSION));
        // A second reconcile is a no-op now that it's stamped.
        assert!(!reconcile_processing_version(&mut s).changed);
    }
}
