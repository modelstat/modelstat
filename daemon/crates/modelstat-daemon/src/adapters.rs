//! Concrete adapters wiring the daemon's SEAMS to the real world: the scan's
//! [`BatchSink`] onto the on-disk [`Spool`], the uploader's [`BatchUploader`] and
//! reconcile's [`BackfillDigest`] onto `modelstat_ingest::DeviceApi`. The seams
//! stay generic + fake-testable; these are the production impls. They reuse
//! `DeviceApi`'s already-tested HTTP methods (`upload_batch` never-drop matrix,
//! `authed_json_get` SPA-safe GET), so the only new logic — the result mapping +
//! URL building — is factored into pure helpers with unit tests.

use modelstat_ingest::{DeviceApi, UploadResult};
use modelstat_pipeline::{
    build_for_one_session, BuildOutcome, Embedder, ResilientSummarizer, Summarizer,
};
use modelstat_receiver::PipelineRunner;
use modelstat_redact::PiiModel;
use modelstat_wire::{IngestBatch, RawEvent, Segment};

use crate::anchors::AnchorMiner;
use crate::flush::PreparedBatch;
use crate::reconcile::{BackfillDaySessions, BackfillDays, BackfillDigest};
use crate::scan::{BatchSink, Hold};
use crate::spool::{Spool, SpoolError};
use crate::uploader::{BatchUploader, Hold as UploadHold};

/// Map an `upload_batch` result to the never-drop outcome the drain expects: a
/// confirmed commit → the server-accepted count; anything else → HOLD (the
/// spooled file stays put and the next pass retries it). Logged loudly.
fn upload_outcome(result: UploadResult) -> Result<u64, UploadHold> {
    match result {
        UploadResult::Commit(resp) => Ok(resp.accepted),
        UploadResult::Hold(reason) => {
            modelstat_log::log_warn!("batch upload held — {reason}");
            Err(UploadHold)
        }
    }
}

impl BatchSink for Spool {
    fn accept(&self, batch: &PreparedBatch) -> Result<(), Hold> {
        match self.push(&batch.batch, batch.raw, batch.segment_count) {
            Ok(_) => Ok(()),
            // Backpressure, not corruption: the scan stops advancing cursors, the
            // transcripts stay unread on disk, and the next cycle picks up exactly
            // where this one stopped once the uploader has drained something.
            Err(e @ SpoolError::Full { .. }) => {
                modelstat_log::log_warn!("{e} — pausing the scan until the queue drains");
                Err(Hold)
            }
            // A real fault. Loud, and the scan stops rather than advance a cursor
            // past events it cannot prove are safe anywhere.
            Err(e @ SpoolError::Io(_)) => {
                modelstat_log::log_error!("{e} — scanning is paused; nothing has been lost");
                Err(Hold)
            }
        }
    }
}

/// A [`BatchSink`] that stamps the modes a batch was actually BUILT under
/// before parking it. The stamps have to happen here, at the spool door:
/// batches wait in the spool for arbitrarily long, and stamping at upload time
/// would label a batch with whatever the settings are by then — the exact
/// drift these fields exist to make answerable ("was THIS batch scrubbed on
/// the box? WHERE was it summarised?").
///
/// The repo anchors ride the same door for a different reason: they are a
/// property of the REPOS this batch touched, not of the events in it, and this
/// is the one place every batch passes through whatever built it (cloud ships
/// one raw batch per session, local one summarised batch per flush).
pub struct StampedSink<'a> {
    pub spool: &'a Spool,
    pub redactor_mode: String,
    pub summarizer_mode: String,
    /// The pre-AI baseline miner, or None when the device opted out. Mining is
    /// gated per repo per run inside it, so most batches cost one HEAD read and
    /// carry no anchors at all.
    pub anchors: Option<&'a AnchorMiner>,
}

impl BatchSink for StampedSink<'_> {
    fn accept(&self, batch: &PreparedBatch) -> Result<(), Hold> {
        let mut stamped = PreparedBatch {
            batch: batch.batch.clone(),
            raw: batch.raw,
            segment_count: batch.segment_count,
        };
        stamped.batch.redactor_mode = Some(self.redactor_mode.clone());
        stamped.batch.summarizer_mode = Some(self.summarizer_mode.clone());
        stamped.batch.repo_anchors = self
            .anchors
            .and_then(|miner| miner.anchors_for(&stamped.batch.events));
        self.spool.accept(&stamped)
    }
}

/// The backfill-digest endpoint URL: `<api>/v1/backfill/digests[?day=<day>]`
/// (port of `api.ts::backfillGet`). The day is a plain `YYYY-MM-DD` (no chars
/// that need percent-encoding).
fn backfill_url(api_url: &str, day: Option<&str>) -> String {
    let base = api_url.trim_end_matches('/');
    match day {
        Some(d) => format!("{base}/v1/backfill/digests?day={d}"),
        None => format!("{base}/v1/backfill/digests"),
    }
}

impl BatchUploader for DeviceApi {
    async fn upload(&self, batch: &IngestBatch, raw: bool) -> Result<u64, UploadHold> {
        upload_outcome(self.upload_batch(batch, raw).await)
    }
}

impl BackfillDigest for DeviceApi {
    async fn fetch_days(&mut self) -> Option<BackfillDays> {
        let base = self.config().api_url();
        self.authed_json_get::<BackfillDays>(&backfill_url(&base, None))
            .await
    }

    async fn fetch_day_sessions(&mut self, day: &str) -> Option<BackfillDaySessions> {
        let base = self.config().api_url();
        self.authed_json_get::<BackfillDaySessions>(&backfill_url(&base, Some(day)))
            .await
    }
}

/// The SDK-drain [`PipelineRunner`] backed by the REAL segmentation pipeline
/// (`build_for_one_session`: redact → segment → summarise → tag one session).
/// `None` when a session HELD (engine down) — the same no-degrade contract the
/// file-scan path enforces, so the SDK drain never ships a degraded batch either.
/// Borrows the daemon's engine adapters; daemon-main constructs one per drain.
pub struct EnginePipeline<'a, S, E, N> {
    resilient: &'a ResilientSummarizer<S>,
    embedder: &'a E,
    redactor: &'a N,
}

impl<'a, S, E, N> EnginePipeline<'a, S, E, N> {
    pub fn new(resilient: &'a ResilientSummarizer<S>, embedder: &'a E, redactor: &'a N) -> Self {
        EnginePipeline {
            resilient,
            embedder,
            redactor,
        }
    }
}

impl<S, E, N> PipelineRunner for EnginePipeline<'_, S, E, N>
where
    S: Summarizer,
    E: Embedder,
    N: PiiModel,
{
    async fn run(&self, events: &[RawEvent]) -> Option<Vec<Segment>> {
        match build_for_one_session(events, self.resilient, self.embedder, self.redactor).await {
            BuildOutcome::Ready(segments) => Some(segments),
            BuildOutcome::Held => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::AnsweringRedactor;
    use modelstat_ingest::IngestResponse;
    use modelstat_pipeline::NoEmbedder;
    use modelstat_sumclient::{CompleteRequest, SumError};
    use std::time::Duration;

    // A fake engine: healthy → a fixed reply for every pass; failing → 503.
    struct Fake {
        failing: bool,
    }
    impl Summarizer for Fake {
        async fn complete(&self, _req: &CompleteRequest) -> Result<String, SumError> {
            if self.failing {
                Err(SumError::Http(503))
            } else {
                Ok("did the thing".into())
            }
        }
    }

    fn ev(session: &str, ts: &str) -> RawEvent {
        RawEvent {
            content_bytes: None,
            source_event_id: format!("{session}:{ts}"),
            ts: ts.into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: session.into(),
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: None,
            tokens_unmapped: std::collections::BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: Some("hello world".into()),
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    #[tokio::test]
    async fn engine_pipeline_segments_when_healthy_and_holds_when_down() {
        let events = vec![
            ev("s1", "2026-07-16T10:00:00.000Z"),
            ev("s1", "2026-07-16T10:01:00.000Z"),
        ];
        // Healthy → Some(segments).
        let healthy = ResilientSummarizer::with_cooldown(Fake { failing: false }, Duration::ZERO);
        let runner = EnginePipeline::new(&healthy, &NoEmbedder, &AnsweringRedactor);
        let segs = runner.run(&events).await;
        assert!(segs.is_some());
        assert!(!segs.unwrap().is_empty());

        // Engine down → None (HOLD, no degraded batch).
        let down = ResilientSummarizer::with_cooldown(Fake { failing: true }, Duration::ZERO);
        let runner = EnginePipeline::new(&down, &NoEmbedder, &AnsweringRedactor);
        assert!(runner.run(&events).await.is_none());
    }

    #[test]
    fn commit_maps_to_accepted_count_hold_maps_to_hold() {
        let commit = UploadResult::Commit(IngestResponse {
            accepted: 7,
            new_sessions: 0,
            updated_sessions: 0,
            batch_id: String::new(),
            raw_s3_key: None,
        });
        assert_eq!(upload_outcome(commit), Ok(7));
        assert_eq!(
            upload_outcome(UploadResult::Hold("offline".into())),
            Err(UploadHold)
        );
    }

    /// The spool is the scan's sink, and a full one must PUSH BACK rather than
    /// drop: the batch is refused, the caller holds, and the events are still in
    /// the transcript waiting to be re-read.
    #[test]
    fn a_full_spool_holds_the_scan_instead_of_losing_the_batch() {
        let dir =
            std::env::temp_dir().join(format!("modelstat-sink-{}-{}", std::process::id(), "full"));
        let _ = std::fs::remove_dir_all(&dir);
        let spool = Spool::open(&dir, 1).unwrap(); // cap below one batch
        let prepared = PreparedBatch {
            batch: IngestBatch {
                batch_id: "b1".into(),
                device_id: "dev".into(),
                daemon_version: "t".into(),
                events: vec![ev("s1", "2026-07-16T10:00:00.000Z")],
                segments: Vec::new(),
                tool_calls: Vec::new(),
                session_installs: None,
                session_titles: None,
                session_metadata: None,
                summarizer_mode: None,
                redactor_mode: None,
                repo_anchors: None,
            },
            raw: true,
            segment_count: 0,
        };
        // The first fits (the cap is only consulted before a write).
        assert_eq!(spool.accept(&prepared), Ok(()));
        assert_eq!(spool.accept(&prepared), Err(Hold));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backfill_url_appends_day_and_trims_trailing_slash() {
        assert_eq!(
            backfill_url("https://api.modelstat.ai/", None),
            "https://api.modelstat.ai/v1/backfill/digests"
        );
        assert_eq!(
            backfill_url("https://api.modelstat.ai", Some("2026-07-10")),
            "https://api.modelstat.ai/v1/backfill/digests?day=2026-07-10"
        );
    }
}
