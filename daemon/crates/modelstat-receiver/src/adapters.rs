//! Wires the SDK-drain [`DrainUploader`] seam to the real
//! `modelstat_ingest::DeviceApi`. The daemon-main tick calls `drain_local_queue`
//! with a `DeviceApi`; the drain ships each built batch to `/v1/ingest`
//! (`raw = false` — the daemon already produced local segments). Reuses
//! `upload_batch`'s never-drop matrix, so a non-commit HOLDS the batch (its
//! events stay durably queued for the next tick).

use modelstat_ingest::{DeviceApi, HoldScope, UploadResult};
use modelstat_wire::IngestBatch;

use crate::ingest::{DrainUploader, Hold};

fn upload_outcome(result: UploadResult) -> Result<u64, Hold> {
    match result {
        UploadResult::Commit(resp) => Ok(resp.accepted),
        // The SDK drain rebuilds its batches from the durable local queue each
        // tick rather than working a FIFO of finished files, so it has no
        // head-of-line problem to solve and needs no scope split here — but a
        // content refusal is still a contract mismatch, not a blip, so it is
        // logged as an error.
        UploadResult::Hold { reason, scope } => {
            if scope == HoldScope::Batch {
                modelstat_log::log_error!(
                    "the server REFUSED an SDK batch on its content — {reason}. Its events \
                     stay queued; this is a daemon/server contract mismatch."
                );
            } else {
                modelstat_log::log_warn!("SDK drain upload held — {reason}");
            }
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
        assert_eq!(
            upload_outcome(UploadResult::Hold {
                reason: "5xx".into(),
                scope: HoldScope::Wire,
            }),
            Err(Hold)
        );
    }
}
