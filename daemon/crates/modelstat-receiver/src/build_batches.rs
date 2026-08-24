//! `build_batches` — the queue → `IngestBatch` builder, a port of the retired
//! TypeScript daemon's `buildBatches` + `finalise`. One
//! pass over the durable [`QueueStore`](crate::queue::QueueStore): gather unsent
//! events per session, debounce sessions still producing, split before the event
//! / tool-call caps, run the segmentation pipeline per session, and assemble the
//! batches the drain loop uploads (then `mark_sent` on commit).
//!
//! This is the SDK-queue analogue of the file-scan path's `build_flush_batches`
//! (both attribute tool calls via the shared `attach_segment_ids` + mint a random
//! `batch_id`). The one behavioural divergence from the TS is no-degrade: the
//! [`PipelineRunner`] can HOLD (engine down), and any held session holds the whole
//! pass — the events stay queued and retry, never shipping a degraded batch.

use std::collections::BTreeMap;

use modelstat_ingest::accounts::{session_installs_for, Accounts};
use modelstat_ingest::{device_timezone, device_tz_offset_minutes};
use modelstat_parsers::{GitEnrichment, ToolCallDraft};
use modelstat_pipeline::{attach_segment_ids, batch_id, build_session_metadata};
use modelstat_wire::{IngestBatch, RawEvent, Segment};

use crate::queue::QueueStore;

/// Ship a session once it's been quiet this long (still-producing sessions wait).
pub const SESSION_DEBOUNCE_MS: i64 = 7_000;
/// Default per-batch event cap (mirrors `INGEST_BATCH_MAX_EVENTS`).
pub const INGEST_BATCH_MAX_EVENTS: usize = 1_000;
/// Ship a busy session immediately once it has this many unsent events, even if
/// it hasn't gone quiet — so a long active session still drains.
pub const FORCE_SHIP_THRESHOLD: usize = 200;
/// Hard per-batch tool-call cap — the server 400s a batch above it, so split first.
pub const INGEST_BATCH_MAX_TOOL_CALLS: usize = 20_000;

/// Segment one session's events into `Segment`s. `None` = the engine is
/// unavailable → HOLD (no-degrade: keep the events queued and retry), matching
/// the file-scan path's `build_for_one_session` Held outcome. The receiver backs
/// this with the real pipeline; tests use a fake.
// In-crate seam only (the receiver + tests implement it); no caller ever needs
// `Send` bounds on the futures, so plain `async fn` stays the clearer spelling.
#[allow(async_fn_in_trait)]
pub trait PipelineRunner {
    async fn run(&self, events: &[RawEvent]) -> Option<Vec<Segment>>;
}

/// The outcome of one drain-build pass.
pub enum DrainBatches {
    /// Batches ready to upload (possibly empty when every session is debouncing).
    Ready(Vec<IngestBatch>),
    /// A session's pipeline held (engine down) — ship nothing this pass.
    Held,
}

/// Tunables for one `build_batches` pass. [`BuildBatchesOpts::new`] fills the
/// TS defaults; tests override `max_events` to force splits without 1000 events.
pub struct BuildBatchesOpts {
    pub device_id: String,
    pub daemon_version: String,
    pub now_ms: i64,
    pub debounce_ms: i64,
    pub max_events: usize,
    pub force_ship_threshold: usize,
    /// Which provider account is logged in, and since when — the snapshot each
    /// shipped session is named against. Empty (the `new` default) names
    /// nothing, leaving the server to infer exactly as before.
    pub accounts: Accounts,
}

impl BuildBatchesOpts {
    pub fn new(
        device_id: impl Into<String>,
        daemon_version: impl Into<String>,
        now_ms: i64,
    ) -> Self {
        BuildBatchesOpts {
            device_id: device_id.into(),
            daemon_version: daemon_version.into(),
            now_ms,
            debounce_ms: SESSION_DEBOUNCE_MS,
            max_events: INGEST_BATCH_MAX_EVENTS,
            force_ship_threshold: FORCE_SHIP_THRESHOLD,
            accounts: Accounts::new(),
        }
    }
}

/// One pass through the queue. Port of `buildBatches`.
pub async fn build_batches<Q, P>(
    store: &Q,
    pipeline: &P,
    opts: &BuildBatchesOpts,
    // The session-metadata git seam (channels 2/5/6), reborrowed per `finalise`.
    // `None` disables the on-disk git channels — the content channel still mines
    // the turns + abstracts, so an SDK with no local checkout still gets refs.
    mut git: Option<&mut (dyn GitEnrichment + Send)>,
) -> DrainBatches
where
    Q: QueueStore,
    P: PipelineRunner,
{
    let by_session = store.list_unsent_by_session().await;
    let mut batches: Vec<IngestBatch> = Vec::new();
    let mut events_cur: Vec<RawEvent> = Vec::new();
    let mut session_ids_cur: Vec<String> = Vec::new();
    let mut calls_cur: Vec<ToolCallDraft> = Vec::new();

    for items in by_session.values() {
        if items.is_empty() {
            continue;
        }
        // Debounce: skip sessions still producing events, unless over the
        // force-ship threshold.
        let last_ts = items.iter().map(|i| i.last_event_ts_ms).max().unwrap_or(0);
        let quiet = opts.now_ms - last_ts >= opts.debounce_ms;
        if !quiet && items.len() < opts.force_ship_threshold {
            continue;
        }

        for item in items {
            let item_calls = item.tool_calls.clone().unwrap_or_default();
            // Split BEFORE either cap overflows. A tool call always ships in the
            // same batch as its emitting event, so the tool-call check runs
            // pre-push; the non-empty guard lets a pathological single event with
            // >cap calls still ship alone (a parser bug, not a batching one).
            if events_cur.len() >= opts.max_events
                || (!events_cur.is_empty()
                    && calls_cur.len() + item_calls.len() > INGEST_BATCH_MAX_TOOL_CALLS)
            {
                match finalise(
                    &events_cur,
                    &session_ids_cur,
                    &calls_cur,
                    pipeline,
                    &opts.device_id,
                    &opts.daemon_version,
                    git.as_deref_mut(),
                    &opts.accounts,
                )
                .await
                {
                    Some(b) => batches.push(b),
                    None => return DrainBatches::Held,
                }
                events_cur.clear();
                session_ids_cur.clear();
                calls_cur.clear();
            }
            events_cur.push(item.event.clone());
            session_ids_cur.push(item.session_id.clone());
            calls_cur.extend(item_calls);
        }
    }
    if !events_cur.is_empty() {
        match finalise(
            &events_cur,
            &session_ids_cur,
            &calls_cur,
            pipeline,
            &opts.device_id,
            &opts.daemon_version,
            git,
            &opts.accounts,
        )
        .await
        {
            Some(b) => batches.push(b),
            None => return DrainBatches::Held,
        }
    }
    DrainBatches::Ready(batches)
}

/// Assemble one batch: run the pipeline per session (segments never span session
/// boundaries), then attribute tool calls to the segments over the same events.
/// `None` = a session's pipeline held. Port of `finalise`.
// `'g` (the borrow) is deliberately split from `'o` (the erased handle's own
// lifetime). Written as one `&mut (dyn … + Send)` they unify, and the per-call
// reborrow below would then have to last as long as the ORIGINAL handle — which
// the loop can't give, since it reborrows once per batch.
// Eight named scalars/seams, each documented above; a param struct would only
// re-spell the same list one hop away.
#[allow(clippy::too_many_arguments)]
async fn finalise<'g, 'o: 'g, P: PipelineRunner>(
    events: &[RawEvent],
    session_ids: &[String],
    calls: &[ToolCallDraft],
    pipeline: &P,
    device_id: &str,
    daemon_version: &str,
    git: Option<&'g mut (dyn GitEnrichment + Send + 'o)>,
    accounts: &Accounts,
) -> Option<IngestBatch> {
    let mut by_session: BTreeMap<String, Vec<RawEvent>> = BTreeMap::new();
    // `session_ids[i]` is authoritative — it may override what the turn carries,
    // so the metadata pass (which keys on `RawEvent.session_id`) has to see the
    // resolved id or it would file a session's references under the stale one.
    let mut resolved: Vec<RawEvent> = Vec::with_capacity(events.len());
    for (i, ev) in events.iter().enumerate() {
        let sid = session_ids
            .get(i)
            .cloned()
            .unwrap_or_else(|| ev.session_id.clone());
        let mut e = ev.clone();
        e.session_id.clone_from(&sid);
        resolved.push(e);
        by_session.entry(sid).or_default().push(ev.clone());
    }
    let mut segments: Vec<Segment> = Vec::new();
    for sess_events in by_session.values() {
        match pipeline.run(sess_events).await {
            Some(segs) => segments.extend(segs),
            None => return None, // engine down → hold the whole pass
        }
    }
    // The same join layer between AI spend and shipped work the file-scan path
    // ships. Built BEFORE the drain strips `content_excerpt`, so the content
    // channel still sees the turns; only public reference shapes (slugs, PR /
    // issue numbers, repo-relative paths) end up on the wire — never the text
    // itself, which is exactly what that strip protects.
    let session_metadata = build_session_metadata(&segments, &resolved, git, None).await;
    // Segments are in hand, so attribution happens at the last responsible moment.
    let tool_calls = attach_segment_ids(calls, &segments);
    Some(IngestBatch {
        batch_id: batch_id(),
        device_id: device_id.into(),
        daemon_version: daemon_version.into(),
        events: events.to_vec(),
        segments,
        tool_calls,
        // Keyed on the session ids the SHIPPED events carry (`events`, not
        // `resolved`) — that is what the server looks these up by.
        session_installs: session_installs_for(events, accounts),
        session_actors: None,
        session_titles: None,
        // Absent (not an empty map) when nothing was found — an empty one would
        // only overwrite better server state.
        session_metadata: if session_metadata.is_empty() {
            None
        } else {
            serde_json::to_value(&session_metadata).ok()
        },
        summarizer_mode: None,
        // The zone this machine is in, as it is at the moment the batch is
        // built. Stamped at build rather than at upload: a batch can wait in the
        // spool for hours, and the offset that belongs to it is the one that was
        // in force when its events were gathered. The NAME and the offset both,
        // verbatim from the OS — an OS that states no zone ships None, never a
        // guess. The receiver is the LOOPBACK door, so this machine IS the
        // device whose SDK events these are.
        tz: device_timezone(),
        tz_offset_minutes: device_tz_offset_minutes(),
        redactor_mode: None,
        // The receiver takes turns over HTTP rather than re-reading a
        // transcript, so it has no generation to supersede: every batch is an
        // append by construction (core#701).
        segment_generations: None,
        repo_anchors: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{FileQueueStore, QueueItem};
    use modelstat_wire::{RedactionReport, TokenUsage};
    use std::path::PathBuf;

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "modelstat-bb-{}-{tag}/queue.json",
            std::process::id()
        ))
    }

    fn raw_event(id: &str, session: &str) -> RawEvent {
        RawEvent {
            seq: None,
            started_at: None,
            first_token_at: None,
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            source_event_id: id.into(),
            ts: "2026-07-16T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: session.into(),
            actor_id: None,
            recipient_actor_id: None,
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: None,
            tokens_unmapped: std::collections::BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            tool_paths: Vec::new(),
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    fn item(id: &str, session: &str, ts_ms: i64) -> QueueItem {
        QueueItem {
            source_event_id: id.into(),
            session_id: session.into(),
            agent: "claude_code".into(),
            event: raw_event(id, session),
            last_event_ts_ms: ts_ms,
            synced: false,
            sent_batch_id: None,
            tool_calls: None,
        }
    }

    /// Same as [`item`], but the turn's text carries a forge reference for the
    /// content channel to mine.
    fn item_saying(id: &str, session: &str, ts_ms: i64, excerpt: &str) -> QueueItem {
        let mut it = item(id, session, ts_ms);
        it.event.content_excerpt = Some(excerpt.into());
        it
    }

    #[tokio::test]
    async fn sdk_batch_ships_session_metadata() {
        // The SDK path had the same hole as cloud: `session_metadata: None`. With
        // no git seam (None) the content channel still mines the turns, so an SDK
        // host with no local checkout still gets its references.
        let path = tmp_path("sdk_meta");
        let q = seed(
            &path,
            vec![item_saying(
                "e1",
                "s1",
                0,
                "shipped in https://github.com/acme/web/pull/7",
            )],
        )
        .await;
        let out = build_batches(
            &q,
            &FakePipeline { held: false },
            &BuildBatchesOpts::new("dev1", "9.9.9", 10_000),
            None,
        )
        .await;
        let DrainBatches::Ready(b) = out else {
            panic!("ready")
        };
        let meta = b[0]
            .session_metadata
            .clone()
            .expect("the SDK batch must carry session_metadata");
        assert_eq!(meta["s1"]["pull_requests"][0]["number"], 7);
    }

    /// Every instant in a batch is UTC, so the batch itself has to say what zone
    /// the machine was in — otherwise nothing downstream can tell a 09:00 start
    /// in one zone from a 09:00 start seven hours away. Stamped at BUILD, so a
    /// batch that waits in the spool keeps the offset its events were gathered
    /// under rather than the one in force when it finally uploads.
    #[tokio::test]
    async fn a_built_batch_states_the_zone_its_machine_was_in() {
        let path = tmp_path("sdk_tz");
        let q = seed(&path, vec![item("e1", "s1", 0)]).await;
        let out = build_batches(
            &q,
            &FakePipeline { held: false },
            &BuildBatchesOpts::new("dev1", "9.9.9", 10_000),
            None,
        )
        .await;
        let DrainBatches::Ready(b) = out else {
            panic!("ready")
        };
        // The contract pair the server reads: the IANA NAME and the offset,
        // exactly as the OS states them — None where the OS states none, never
        // a guess.
        assert_eq!(b[0].tz, device_timezone());
        assert_eq!(b[0].tz_offset_minutes, device_tz_offset_minutes());
        let offset = b[0]
            .tz_offset_minutes
            .expect("a batch this device built must say what zone it was in");
        assert!(
            (-840..=840).contains(&offset),
            "{offset} is outside the wire's range"
        );
    }

    #[tokio::test]
    async fn sdk_batch_omits_metadata_when_no_references_are_found() {
        let path = tmp_path("sdk_meta_none");
        let q = seed(&path, vec![item("e1", "s1", 0)]).await;
        let out = build_batches(
            &q,
            &FakePipeline { held: false },
            &BuildBatchesOpts::new("dev1", "9.9.9", 10_000),
            None,
        )
        .await;
        let DrainBatches::Ready(b) = out else {
            panic!("ready")
        };
        assert!(b[0].session_metadata.is_none());
    }

    fn segment_over(session: &str, source_ids: &[&str]) -> Segment {
        Segment {
            segment_id: format!("seg-{session}"),
            session_id: session.into(),
            agent: "claude_code".into(),
            started_at: "2026-07-16T10:00:00.000Z".into(),
            ended_at: "2026-07-16T10:05:00.000Z".into(),
            r#abstract: "did stuff".into(),
            tokens: TokenUsage::default(),
            tags: Vec::new(),
            redaction: RedactionReport::default(),
            source_event_ids: source_ids.iter().map(|s| s.to_string()).collect(),
            abstract_embedding: None,
            behavior: None,
            user_intent: None,
            local_time: None,
        }
    }

    // A pipeline that emits one segment covering the run's events, or holds.
    struct FakePipeline {
        held: bool,
    }
    impl PipelineRunner for FakePipeline {
        async fn run(&self, events: &[RawEvent]) -> Option<Vec<Segment>> {
            if self.held {
                return None;
            }
            let session = events.first().map(|e| e.session_id.as_str()).unwrap_or("s");
            let ids: Vec<&str> = events.iter().map(|e| e.source_event_id.as_str()).collect();
            Some(vec![segment_over(session, &ids)])
        }
    }

    async fn seed(path: &PathBuf, items: Vec<QueueItem>) -> FileQueueStore {
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let q = FileQueueStore::new(path);
        for it in items {
            q.put(it).await.unwrap();
        }
        q
    }

    #[tokio::test]
    async fn ships_a_quiet_session() {
        let path = tmp_path("quiet");
        let q = seed(&path, vec![item("e1", "s1", 0), item("e2", "s1", 0)]).await;
        // now (10s) − lastTs (0) ≥ debounce (7s) → quiet → ships.
        let out = build_batches(
            &q,
            &FakePipeline { held: false },
            &BuildBatchesOpts::new("dev1", "9.9.9", 10_000),
            None,
        )
        .await;
        match out {
            DrainBatches::Ready(b) => {
                assert_eq!(b.len(), 1);
                assert_eq!(b[0].events.len(), 2);
                assert_eq!(b[0].device_id, "dev1");
                assert!(!b[0].segments.is_empty());
            }
            DrainBatches::Held => panic!("healthy pipeline must not hold"),
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn debounces_a_still_producing_small_session() {
        let path = tmp_path("debounce");
        // lastTs 9500, now 10000 → 500ms < 7s debounce, and 1 item < force threshold.
        let q = seed(&path, vec![item("e1", "s1", 9_500)]).await;
        let out = build_batches(
            &q,
            &FakePipeline { held: false },
            &BuildBatchesOpts::new("dev1", "9.9.9", 10_000),
            None,
        )
        .await;
        match out {
            DrainBatches::Ready(b) => assert!(b.is_empty()), // held back by debounce
            DrainBatches::Held => panic!("debounce is not a hold"),
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn force_ships_a_busy_session_over_threshold() {
        let path = tmp_path("force");
        // Not quiet (recent), but ≥ force threshold → ships anyway.
        let mut items = Vec::new();
        for i in 0..3 {
            items.push(item(&format!("e{i}"), "s1", 9_900));
        }
        let q = seed(&path, items).await;
        let mut opts = BuildBatchesOpts::new("dev1", "9.9.9", 10_000);
        opts.force_ship_threshold = 3; // lower it so 3 events force-ship
        let out = build_batches(&q, &FakePipeline { held: false }, &opts, None).await;
        assert!(matches!(out, DrainBatches::Ready(b) if b.len() == 1));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn splits_at_the_event_cap() {
        let path = tmp_path("split");
        let q = seed(
            &path,
            vec![
                item("e1", "s1", 0),
                item("e2", "s1", 0),
                item("e3", "s1", 0),
            ],
        )
        .await;
        let mut opts = BuildBatchesOpts::new("dev1", "9.9.9", 10_000);
        opts.max_events = 2; // force a split: [e1,e2] then [e3]
        let out = build_batches(&q, &FakePipeline { held: false }, &opts, None).await;
        match out {
            DrainBatches::Ready(b) => {
                assert_eq!(b.len(), 2);
                assert_eq!(b[0].events.len(), 2);
                assert_eq!(b[1].events.len(), 1);
            }
            DrainBatches::Held => panic!(),
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn a_held_pipeline_holds_the_whole_pass() {
        let path = tmp_path("held");
        let q = seed(&path, vec![item("e1", "s1", 0)]).await;
        let out = build_batches(
            &q,
            &FakePipeline { held: true },
            &BuildBatchesOpts::new("dev1", "9.9.9", 10_000),
            None,
        )
        .await;
        assert!(matches!(out, DrainBatches::Held));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn tool_calls_are_attributed_to_their_segment() {
        let path = tmp_path("attr");
        let mut it = item("e1", "s1", 0);
        // One tool call whose source_event_id matches the event → gets the segment.
        it.tool_calls = Some(vec![draft("e1")]);
        let q = seed(&path, vec![it]).await;
        let out = build_batches(
            &q,
            &FakePipeline { held: false },
            &BuildBatchesOpts::new("dev1", "9.9.9", 10_000),
            None,
        )
        .await;
        match out {
            DrainBatches::Ready(b) => {
                assert_eq!(b[0].tool_calls.len(), 1);
                // Attributed to the fake pipeline's segment (seg-s1), not null.
                assert_eq!(b[0].tool_calls[0].segment_id.as_deref(), Some("seg-s1"));
            }
            DrainBatches::Held => panic!(),
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // A minimal ToolCallDraft anchored on the given source event.
    fn draft(source_event_id: &str) -> ToolCallDraft {
        ToolCallDraft {
            external_call_id: "c1".into(),
            session_id: "s1".into(),
            source_event_id: source_event_id.into(),
            agent: "claude_code".into(),
            server: "local".into(),
            name: "Bash".into(),
            turn_index: None,
            call_index: 0,
            started_at: "2026-07-16T10:00:00.000Z".into(),
            ended_at: None,
            status: "ok".into(),
            args_hash: "deadbeef".into(),
            signature_hash: String::new(),
            args_bytes: 0,
            result_bytes: 0,
            model: None,
            action: None,
        }
    }
}
