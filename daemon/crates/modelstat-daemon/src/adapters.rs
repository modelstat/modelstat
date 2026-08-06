//! Concrete adapters wiring the scan + reconcile SEAMS to the real
//! `modelstat_ingest::DeviceApi`. The daemon-main loop constructs a `DeviceApi`
//! and hands it to `run_scan_over_jobs` (as a [`BatchUploader`]) and
//! `reconcile_backfill` (as a [`BackfillDigest`]); the seams stay generic +
//! fake-testable, these are the production impls. Both reuse `DeviceApi`'s
//! already-tested HTTP methods (`upload_batch` never-drop matrix,
//! `authed_json_get` SPA-safe GET), so the only new logic — the result mapping +
//! URL building — is factored into pure helpers with unit tests.

use modelstat_ingest::{DeviceApi, UploadResult};
use modelstat_pipeline::{
    build_for_one_session, BuildOutcome, Embedder, ResilientSummarizer, Summarizer,
};
use modelstat_receiver::PipelineRunner;
use modelstat_redact::NerModel;
use modelstat_wire::{IngestBatch, RawEvent, Segment};

use crate::reconcile::{BackfillDaySessions, BackfillDays, BackfillDigest};
use crate::scan::{BatchUploader, Hold};

/// Map an `upload_batch` result to the never-drop outcome the scan loop expects:
/// a confirmed commit → the server-accepted count; anything else → HOLD (the
/// cursor stays put, the batch re-ships next cycle). The reason is logged loudly.
fn upload_outcome(result: UploadResult) -> Result<u64, Hold> {
    match result {
        UploadResult::Commit(resp) => Ok(resp.accepted),
        UploadResult::Hold(reason) => {
            modelstat_log::log_warn!("batch upload held — {reason}");
            Err(Hold)
        }
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
    async fn upload(&self, batch: &IngestBatch, raw: bool) -> Result<u64, Hold> {
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
    ner: &'a N,
}

impl<'a, S, E, N> EnginePipeline<'a, S, E, N> {
    pub fn new(resilient: &'a ResilientSummarizer<S>, embedder: &'a E, ner: &'a N) -> Self {
        EnginePipeline {
            resilient,
            embedder,
            ner,
        }
    }
}

impl<S, E, N> PipelineRunner for EnginePipeline<'_, S, E, N>
where
    S: Summarizer,
    E: Embedder,
    N: NerModel,
{
    async fn run(&self, events: &[RawEvent]) -> Option<Vec<Segment>> {
        match build_for_one_session(events, self.resilient, self.embedder, self.ner).await {
            BuildOutcome::Ready(segments) => Some(segments),
            BuildOutcome::Held => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::AnsweringNer;
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
        let runner = EnginePipeline::new(&healthy, &NoEmbedder, &AnsweringNer);
        let segs = runner.run(&events).await;
        assert!(segs.is_some());
        assert!(!segs.unwrap().is_empty());

        // Engine down → None (HOLD, no degraded batch).
        let down = ResilientSummarizer::with_cooldown(Fake { failing: true }, Duration::ZERO);
        let runner = EnginePipeline::new(&down, &NoEmbedder, &AnsweringNer);
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
            Err(Hold)
        );
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
