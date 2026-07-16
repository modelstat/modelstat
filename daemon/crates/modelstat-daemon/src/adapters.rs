//! Concrete adapters wiring the scan + reconcile SEAMS to the real
//! `modelstat_ingest::DeviceApi`. The daemon-main loop constructs a `DeviceApi`
//! and hands it to `run_scan_over_jobs` (as a [`BatchUploader`]) and
//! `reconcile_backfill` (as a [`BackfillDigest`]); the seams stay generic +
//! fake-testable, these are the production impls. Both reuse `DeviceApi`'s
//! already-tested HTTP methods (`upload_batch` never-drop matrix,
//! `authed_json_get` SPA-safe GET), so the only new logic — the result mapping +
//! URL building — is factored into pure helpers with unit tests.

use modelstat_ingest::{DeviceApi, UploadResult};
use modelstat_wire::IngestBatch;

use crate::reconcile::{BackfillDaySessions, BackfillDays, BackfillDigest};
use crate::scan::{BatchUploader, Hold};

/// Map an `upload_batch` result to the never-drop outcome the scan loop expects:
/// a confirmed commit → the server-accepted count; anything else → HOLD (the
/// cursor stays put, the batch re-ships next cycle). The reason is logged loudly.
fn upload_outcome(result: UploadResult) -> Result<u64, Hold> {
    match result {
        UploadResult::Commit(resp) => Ok(resp.accepted),
        UploadResult::Hold(reason) => {
            eprintln!("modelstat: batch upload held — {reason}");
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
    async fn upload(&mut self, batch: &IngestBatch, raw: bool) -> Result<u64, Hold> {
        upload_outcome(self.upload_batch(batch, raw).await)
    }
}

impl BackfillDigest for DeviceApi {
    async fn fetch_days(&mut self) -> Option<BackfillDays> {
        let base = self.config().api_url();
        self.authed_json_get::<BackfillDays>(&backfill_url(&base, None)).await
    }

    async fn fetch_day_sessions(&mut self, day: &str) -> Option<BackfillDaySessions> {
        let base = self.config().api_url();
        self.authed_json_get::<BackfillDaySessions>(&backfill_url(&base, Some(day)))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_ingest::IngestResponse;

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
        assert_eq!(upload_outcome(UploadResult::Hold("offline".into())), Err(Hold));
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
