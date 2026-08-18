//! The SDK-ingest request logic — batch parse, durable enqueue, and local-queue
//! drain (the axum HTTP layer that calls these lives in `server.rs`).
//! Browser/SDK adapters POST batches to the
//! loopback; we validate, enqueue durably (idempotent), and — on the daemon's
//! tick — drain: build batches, strip the raw excerpt, upload, mark sent.

use modelstat_parsers::{GitEnrichment, ToolCallDraft};
use modelstat_wire::{IngestBatch, RawEvent};
use serde::Deserialize;

use crate::build_batches::{build_batches, BuildBatchesOpts, DrainBatches, PipelineRunner};
use crate::queue::{QueueItem, QueueStore};

/// The incoming SDK batch. `tool_calls` deserialize as `ToolCallDraft` — any
/// `segment_id` the SDK sent is simply ignored (serde drops the unknown field),
/// since attribution is (re)done at batch-build from the daemon's own segments.
#[derive(Debug, Deserialize)]
pub struct WireBatch {
    pub events: Vec<RawEvent>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDraft>,
}

/// Structurally validate a POST body into a [`WireBatch`], or an error string
/// (the caller replies `400`). Port of `parseBatch`. Rust's serde already
/// requires the 6 non-optional `RawEvent` fields (a superset of the TS's 4-field
/// check — the SDK always sends `kind`/`provider`, so this never diverges in
/// practice and rejects a genuinely-malformed event the TS would have queued).
pub fn parse_batch(body: &[u8]) -> Result<WireBatch, String> {
    let batch: WireBatch =
        serde_json::from_slice(body).map_err(|e| format!("invalid batch: {e}"))?;
    if batch.events.is_empty() {
        return Err("events must be a non-empty array".into());
    }
    if batch.events.len() > 10_000 {
        return Err("too many events (max 10000)".into());
    }
    for e in &batch.events {
        for (k, v) in [
            ("source_event_id", &e.source_event_id),
            ("session_id", &e.session_id),
            ("agent", &e.agent),
            ("ts", &e.ts),
        ] {
            if v.is_empty() {
                return Err(format!("event.{k} is required"));
            }
        }
    }
    Ok(batch)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `Date.parse(ts) || Date.now()` — RFC3339 → epoch-ms, else now. The `!= 0`
/// filter mirrors JS treating `0` (the epoch) as falsy, so a `1970` ts also
/// falls back to now (a faithful edge, never hit in practice).
fn event_ts_ms(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp_millis())
        .filter(|ms| *ms != 0)
        .unwrap_or_else(now_ms)
}

/// Enqueue one batch's events durably. Idempotent — [`QueueStore`] dedupes by
/// `source_event_id`, so a retried POST is a no-op. Returns the event count.
/// Port of `enqueue`.
pub async fn enqueue<Q: QueueStore>(store: &Q, batch: &WireBatch) -> std::io::Result<usize> {
    for event in &batch.events {
        // Each event carries only the tool calls born from IT (matched on
        // source_event_id) — same batch as their emitting event server-side.
        let calls: Vec<ToolCallDraft> = batch
            .tool_calls
            .iter()
            .filter(|tc| tc.source_event_id == event.source_event_id)
            .cloned()
            .collect();
        store
            .put(QueueItem {
                source_event_id: event.source_event_id.clone(),
                session_id: event.session_id.clone(),
                agent: event.agent.clone(),
                event: event.clone(),
                last_event_ts_ms: event_ts_ms(&event.ts),
                synced: false,
                sent_batch_id: None,
                tool_calls: if calls.is_empty() { None } else { Some(calls) },
            })
            .await?;
    }
    Ok(batch.events.len())
}

/// The drain-upload seam. `Ok(accepted)` = a confirmed commit (mark the events
/// sent); `Err(Hold)` = a non-commit (leave them queued, retry next tick). The
/// daemon backs this with `DeviceApi::upload_batch`; tests use a fake.
// In-crate seam only (the receiver + tests implement it); no caller ever needs
// `Send` bounds on the futures, so plain `async fn` stays the clearer spelling.
#[allow(async_fn_in_trait)]
pub trait DrainUploader {
    async fn upload(&mut self, batch: &IngestBatch) -> Result<u64, Hold>;
}

/// A non-commit upload — the batch is held, its events stay queued for retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold;

/// What one drain pass accomplished.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DrainResult {
    pub batches: usize,
    pub events: usize,
    /// A pipeline or an upload held — some events stayed queued for retry.
    pub held: bool,
}

/// One drain pass: build batches from the durable queue, strip the raw per-turn
/// excerpt (only redacted segment abstracts leave the machine), upload under the
/// device secret, and mark the shipped events sent. On a held pipeline (engine
/// down) or a held upload, the events stay durably queued and the next tick
/// retries — good data is never dropped. Port of `drainLocalQueue`.
///
/// The caller must not run two drains concurrently (they'd rebuild + double-ship
/// the same batches); the daemon's tick loop is sequential.
pub async fn drain_local_queue<Q, P, U>(
    store: &Q,
    pipeline: &P,
    uploader: &mut U,
    device_id: &str,
    daemon_version: &str,
    now_ms: i64,
    // The session-metadata git seam, forwarded to the batch builder. `None` is a
    // valid wiring (tests, and any host with no local checkout to resolve).
    git: Option<&mut (dyn GitEnrichment + Send)>,
) -> std::io::Result<DrainResult>
where
    Q: QueueStore,
    P: PipelineRunner,
    U: DrainUploader,
{
    if store.count_unsent().await == 0 {
        return Ok(DrainResult::default());
    }
    let mut opts = BuildBatchesOpts::new(device_id, daemon_version, now_ms);
    // Name each shipped session's account, exactly as the file-scan path does.
    // Read once per drain: the heartbeat rewrites this every 10s, and one drain
    // must not straddle a switch.
    opts.accounts = modelstat_ingest::accounts::load_accounts();
    let batches = match build_batches(store, pipeline, &opts, git).await {
        DrainBatches::Held => {
            return Ok(DrainResult {
                held: true,
                ..Default::default()
            })
        }
        DrainBatches::Ready(b) => b,
    };
    let mut result = DrainResult::default();
    for batch in &batches {
        // Privacy: the daemon already produced redacted segment abstracts; the
        // per-event turn excerpt is summariser input only and must never leave the
        // machine (honours "raw text never leaves" even under redaction "none").
        let mut shipped = batch.clone();
        for e in &mut shipped.events {
            e.content_excerpt = None;
        }
        match uploader.upload(&shipped).await {
            Ok(_accepted) => {
                let ids: Vec<String> = batch
                    .events
                    .iter()
                    .map(|e| e.source_event_id.clone())
                    .collect();
                store.mark_sent(&ids, Some(&batch.batch_id)).await?;
                result.batches += 1;
                result.events += batch.events.len();
            }
            // Held: stop here, leave the rest queued (re-sent idempotently next tick).
            Err(Hold) => {
                result.held = true;
                break;
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_batches::PipelineRunner;
    use crate::queue::FileQueueStore;
    use modelstat_wire::{RedactionReport, Segment, TokenUsage};
    use std::path::PathBuf;

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "modelstat-ingest-{}-{tag}/queue.json",
            std::process::id()
        ))
    }

    // A pipeline emitting one segment over the run's events (or holding).
    struct FakePipeline {
        held: bool,
    }
    impl PipelineRunner for FakePipeline {
        async fn run(&self, events: &[RawEvent]) -> Option<Vec<Segment>> {
            if self.held {
                return None;
            }
            let session = events
                .first()
                .map(|e| e.session_id.clone())
                .unwrap_or_default();
            Some(vec![Segment {
                segment_id: format!("seg-{session}"),
                session_id: session,
                agent: "claude_code".into(),
                started_at: "2026-07-16T10:00:00.000Z".into(),
                ended_at: "2026-07-16T10:05:00.000Z".into(),
                r#abstract: "did stuff".into(),
                tokens: TokenUsage::default(),
                tags: Vec::new(),
                redaction: RedactionReport::default(),
                source_event_ids: events.iter().map(|e| e.source_event_id.clone()).collect(),
                abstract_embedding: None,
                behavior: None,
                user_intent: None,
                local_time: None,
            }])
        }
    }

    // Records uploaded batches (with their post-strip event excerpts), or holds.
    #[derive(Default)]
    struct FakeUploader {
        uploaded: Vec<IngestBatch>,
        hold: bool,
    }
    impl DrainUploader for FakeUploader {
        async fn upload(&mut self, batch: &IngestBatch) -> Result<u64, Hold> {
            if self.hold {
                return Err(Hold);
            }
            let n = batch.events.len() as u64;
            self.uploaded.push(batch.clone());
            Ok(n)
        }
    }

    fn body(json: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json).unwrap()
    }

    fn ok_event(id: &str) -> serde_json::Value {
        serde_json::json!({
            "source_event_id": id,
            "ts": "2026-07-16T10:00:00.000Z",
            "kind": "message",
            "agent": "claude_code",
            "provider": "anthropic",
            "session_id": "s1",
            "content_excerpt": "secret raw text"
        })
    }

    #[test]
    fn parse_batch_accepts_a_valid_body_and_rejects_bad_ones() {
        let good = parse_batch(&body(serde_json::json!({ "events": [ok_event("e1")] }))).unwrap();
        assert_eq!(good.events.len(), 1);

        assert!(parse_batch(&body(serde_json::json!({ "events": [] })))
            .unwrap_err()
            .contains("non-empty"));
        // Missing a required field (no session_id) → serde rejects.
        assert!(parse_batch(&body(serde_json::json!({
            "events": [{ "source_event_id": "e", "ts": "t", "kind": "message", "agent": "a", "provider": "p" }]
        })))
        .is_err());
        // Present-but-empty required field → the explicit check fires.
        let mut e = ok_event("");
        e["source_event_id"] = serde_json::json!("");
        assert!(parse_batch(&body(serde_json::json!({ "events": [e] })))
            .unwrap_err()
            .contains("source_event_id"));
    }

    /// An SDK built before the money removal still sends `pricing_mode`. The
    /// wire type does not `deny_unknown_fields`, so that batch still parses and
    /// the stale field is dropped — an old SDK keeps working against a current
    /// daemon.
    #[test]
    fn a_stale_pricing_mode_field_is_ignored_not_rejected() {
        let mut e = ok_event("e1");
        e["pricing_mode"] = serde_json::json!("subscription");
        let batch = parse_batch(&body(serde_json::json!({ "events": [e] })))
            .expect("an old SDK's batch must still parse");
        assert_eq!(batch.events.len(), 1);
    }

    #[tokio::test]
    async fn enqueue_is_idempotent() {
        let path = tmp_path("enq");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let q = FileQueueStore::new(&path);
        let batch = parse_batch(&body(serde_json::json!({
            "events": [ok_event("e1"), ok_event("e2")]
        })))
        .unwrap();
        assert_eq!(enqueue(&q, &batch).await.unwrap(), 2);
        assert_eq!(enqueue(&q, &batch).await.unwrap(), 2); // dupes ignored
        assert_eq!(q.count_unsent().await, 2);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn drain_strips_excerpt_uploads_and_marks_sent() {
        let path = tmp_path("drain");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let q = FileQueueStore::new(&path);
        let batch = parse_batch(&body(serde_json::json!({ "events": [ok_event("e1")] }))).unwrap();
        enqueue(&q, &batch).await.unwrap();

        let mut up = FakeUploader::default();
        // now (10s) ≥ debounce so the session ships.
        let r = drain_local_queue(
            &q,
            &FakePipeline { held: false },
            &mut up,
            "dev1",
            "9.9.9",
            2_000_000_000_000,
            None,
        )
        .await
        .unwrap();
        assert_eq!(r.batches, 1);
        assert_eq!(r.events, 1);
        assert!(!r.held);
        // The raw excerpt was stripped before upload.
        assert!(up.uploaded[0].events[0].content_excerpt.is_none());
        // Events marked sent → nothing left to drain.
        assert_eq!(q.count_unsent().await, 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn drain_holds_and_keeps_events_queued_when_upload_holds() {
        let path = tmp_path("hold");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let q = FileQueueStore::new(&path);
        let batch = parse_batch(&body(serde_json::json!({ "events": [ok_event("e1")] }))).unwrap();
        enqueue(&q, &batch).await.unwrap();

        let mut up = FakeUploader {
            hold: true,
            ..Default::default()
        };
        let r = drain_local_queue(
            &q,
            &FakePipeline { held: false },
            &mut up,
            "dev1",
            "9.9.9",
            2_000_000_000_000,
            None,
        )
        .await
        .unwrap();
        assert!(r.held);
        assert_eq!(r.batches, 0);
        // Never-drop: still queued for the next tick.
        assert_eq!(q.count_unsent().await, 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn drain_of_empty_queue_is_a_noop() {
        let path = tmp_path("empty");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let q = FileQueueStore::new(&path);
        let mut up = FakeUploader::default();
        let r = drain_local_queue(
            &q,
            &FakePipeline { held: false },
            &mut up,
            "dev1",
            "9.9.9",
            2_000_000_000_000,
            None,
        )
        .await
        .unwrap();
        assert_eq!(r, DrainResult::default());
        assert!(up.uploaded.is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
