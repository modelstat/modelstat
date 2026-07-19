//! Wires the SDK-drain [`DrainUploader`] seam to the real
//! `modelstat_ingest::DeviceApi`. The daemon-main tick calls `drain_local_queue`
//! with a `DeviceApi`; the drain ships each built batch to `/v1/ingest`
//! (`raw = false` — the daemon already produced local segments). Reuses
//! `upload_batch`'s never-drop matrix, so a non-commit HOLDS the batch (its
//! events stay durably queued for the next tick).

use modelstat_ingest::{DeviceApi, UploadResult};
use modelstat_wire::IngestBatch;

use crate::ingest::{DrainUploader, Hold};

fn upload_outcome(result: UploadResult) -> Result<u64, Hold> {
    match result {
        UploadResult::Commit(resp) => Ok(resp.accepted),
        UploadResult::Hold(reason) => {
            eprintln!("modelstat: SDK drain upload held — {reason}");
            Err(Hold)
        }
    }
}

impl DrainUploader for DeviceApi {
    async fn upload(&mut self, batch: &IngestBatch) -> Result<u64, Hold> {
        // raw = false: the SDK path built local segment abstracts, so this ships
        // to /v1/ingest (not /raw) exactly like the file-scan commit path.
        upload_outcome(self.upload_batch(batch, false).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_ingest::IngestResponse;

    #[test]
    fn commit_maps_to_accepted_hold_maps_to_hold() {
        let commit = UploadResult::Commit(IngestResponse {
            accepted: 3,
            new_sessions: 0,
            updated_sessions: 0,
            batch_id: String::new(),
            raw_s3_key: None,
        });
        assert_eq!(upload_outcome(commit), Ok(3));
        assert_eq!(upload_outcome(UploadResult::Hold("5xx".into())), Err(Hold));
    }
}
