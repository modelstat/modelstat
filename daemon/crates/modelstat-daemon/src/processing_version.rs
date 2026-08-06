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

/// Current local processing-pipeline version. Bump when the pipeline produces
/// materially different output for the same input.
///
/// v16 — the Rust rewrite's cutover value, absorbing the runtime/model swaps the
///       TS never had (the candle BGE embedder + BERT-NER, and the prompt-fed
///       non-determinism of a different engine) in one bump, so every historical
///       session re-scanned once at cutover. (The TS chain ended at v15; see that
///       file for the v1–v15 history.)
/// v17 — codex token accounting fix. Every codex event ever uploaded carries
///       0 tokens: the parser read `payload.input_tokens` but codex nests the
///       counters under `payload.info.last_token_usage`, and an `unwrap_or(0)`
///       turned the miss into a zero. Re-scanning is the ONLY way to recover
///       those numbers — the true counts exist solely in the rollout JSONL on
///       each device, never having reached the server (the log of record faithfully
///       stores the zeros we sent). A re-scan also re-prices every event against
///       the current rate card, which fixes the historical $0.00 costs from the
///       models that had no rate row (core migration 0073).
/// v18 — session metadata on the cloud + SDK paths. Cloud is the DEFAULT mode and
///       its flush branch hardcoded `session_metadata: None`, so the repos / PRs /
///       issues / files a session touched were never sent by anyone — the server's
///       `session_metadata` table held 0 rows, 0 parts, ever. The detection is
///       purely local (event git context, on-disk git, forge refs in the turns), so
///       re-scanning is the ONLY way to recover it for sessions already uploaded:
///       nothing on the server can derive it after the fact. This bump is what
///       makes that automatic — an auto-updated daemon wipes its cursors on first
///       boot and re-processes the world, with no user action.
/// v19 — conversation capture (SPEC 0005). Message excerpts become real bodies:
///       the VERBATIM redacted text of what was said — nothing regexed away, no
///       truncation (the wire cap, raised 320 → 262144, is an extreme
///       malicious-size guard only) — plus `content_bytes`, `turn_index` for
///       claude_code/pi, and Claude Code's own stated `toolUseResult.durationMs`.
///       The server materializes these into the `messages` table and turn-timing
///       columns; everything already uploaded carries only the old 320-char
///       strip-all-code excerpts, so re-scanning the local JSONL corpus is the
///       ONLY way history gains full transcripts. The server dedupes on
///       `(scope, source_event_id)` and ReplacingMergeTree upserts the wider
///       rows over the old ones — the re-scan is pure upgrade.
/// v20 — codex + cursor join conversation capture (SPEC 0005). Codex shipped
///       `content_excerpt: None` on every event, so its sessions produced no
///       `messages` rows and no stance at all; it now carries the typed prompt
///       (`event_msg`/`user_message`) and the assistant's prose (buffered from
///       `event_msg`/`agent_message` onto the usage-bearing `token_count`
///       event, so one event holds both text and tokens, as the other parsers
///       do — codex repeats each message as a `response_item`, which stays
///       text-free so nothing is captured twice). Cursor was worse than empty:
///       it read `ai_code_hashes`, a table current Cursor does not create
///       (absent from every global + workspace DB on a live install), so it
///       could only ever emit nothing; it now reads the real chat store
///       (`cursorDiskKV` bubbles) and emits verbatim user/assistant messages
///       with per-conversation turn ordinals. Both are local-only facts, so a
///       re-scan is the only way sessions already uploaded gain their
///       transcripts.
/// v21 — long turns are actually REDACTED. The on-device NER model carries 512
///       learned positions and errors past them, `classify` mapped that error to
///       "no model", and `ner_redact` reads "no model" as pass-through — so from
///       v19 onward, when turns became verbatim, every turn over ~2,700 chars
///       left the box UNSCRUBBED. (`ner_active` never caught it: it probes with
///       one short sentinel, which always fits.) Inference is now WINDOWED —
///       every token classified, in overlapping passes the model can take, no
///       text shortened — and the cloud path holds per TURN when the model cannot
///       answer, instead of shipping it. The re-scan is the cleanup: the wider
///       rows upsert over the leaked ones by `(scope, source_event_id)`.
/// v22 — redaction never splices mid-word. A model labels SUBWORDS, and the
///       precise-offset splice took the label at face value, so production shipped
///       `eRPC` as `[REDACTED:ORG]PC`, `Bugbot` as `[REDACTED:ORG]ugbot` and
///       `Compose` as `[REDACTED:ORG]mpose` — 27,130 messages carry an ORG marker,
///       almost all of them technical product names rather than anything private.
///       Reads as corruption of the verbatim text SPEC 0005 exists to keep, and the
///       privacy version is worse: a half-redacted name leaks its other half
///       (`Katherine` → `[REDACTED:PER]erine`). Spans now snap OUTWARD to whole
///       words and fuse when they meet, so a marker never sits against an
///       alphanumeric character. The re-scan is the repair.
/// v23 — the redactor is OpenAI Privacy Filter (ONNX), not a general-purpose NER
///       model. What changes in the DATA: emails, phone numbers, addresses,
///       account numbers and API keys are now caught by the model rather than by
///       the deterministic floor alone; organisations and locations are no longer
///       redacted at all, because an org is not private information and redacting
///       `ClickHouse` cost the prompt analytics for nothing (27,130 of 162,159
///       stored messages carried an ORG marker). Re-ships so history is scrubbed by
///       the model that can actually see secrets, and un-marked where the old one
///       was only ever guessing.
pub const PROCESSING_VERSION: i64 = 23;

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
        // The assertion is "reconcile stamps the CURRENT version", not any
        // particular integer — hardcoding it made every legitimate bump red.
        assert_eq!(s.version, Some(PROCESSING_VERSION));
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
