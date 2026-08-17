//! The spool drain — the only thing in the daemon that talks to `/v1/ingest`.
//!
//! Reads [`crate::spool`] oldest-first and ships each batch, deleting it only on
//! a confirmed commit. It runs on its own clock, independent of scanning, which is
//! the whole point: an outage now costs a retry of the POST, not a re-run of the
//! PII model over the same turns.
//!
//! # Retry shape
//!
//! Two nested loops, deliberately.
//!
//! [`modelstat_ingest::DeviceApi::upload_batch`] already retries a handful of
//! times inside one call (reauth, 429/5xx backoff, `Retry-After`). When it
//! finally gives up it returns a hold, and THIS loop backs off and comes back to
//! the same file — forever, with no attempt ceiling. That is the requirement: a
//! machine that is offline for a day resumes exactly where it stopped, having
//! burned no CPU in the meantime.
//!
//! On a hold we stop starting anything new rather than marching through the rest
//! of the queue. A hold means the server is not taking batches; the rest would
//! just be the same failure N more times.
//!
//! # Why the POSTs overlap
//!
//! Batches go out [`UPLOAD_FANOUT`] at a time, not one by one. A single POST
//! spends most of its life waiting on the server, so a strictly sequential drain
//! makes a first-run backlog of thousands of batches take hours of mostly-idle
//! wire. `DeviceApi` has its own adaptive gate that narrows this further when the
//! server pushes back, so this is a ceiling rather than a target.
//!
//! Overlap costs strict FIFO delivery — batches are STARTED oldest-first but may
//! land out of order. That is fine: every id is deterministic and the server
//! upserts, so ordering was never load-bearing, only tidy.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;

use crate::spool::{Spool, SpoolDepth, SpoolEntry};
use modelstat_ingest::HoldScope;
use modelstat_wire::IngestBatch;

/// Batches in flight at once. Matched to the ingest client's own ceiling, so this
/// is never the binding constraint while still bounding how much of the spool is
/// resident at any moment (this many batches, not the whole queue).
const UPLOAD_FANOUT: usize = modelstat_ingest::upload_gate::MAX_CONCURRENCY;

/// First backoff after a held pass.
const BACKOFF_MIN: Duration = Duration::from_secs(2);
/// Ceiling. Five minutes is short enough that a restored network is noticed
/// promptly and long enough that a night offline is a handful of log lines rather
/// than thousands.
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// A confirmed commit, or a hold to retry. Mirrors the scan's old contract so the
/// never-drop matrix in `DeviceApi` is reused verbatim.
///
/// The [`HoldScope`] says whether the REST of the queue may still be tried this
/// pass. It never authorises dropping anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold(pub HoldScope);

/// Ships one batch. Backed by `DeviceApi` in the daemon; a fake in tests.
pub trait BatchUploader {
    /// `Ok(accepted)` = committed (the spooled file may be deleted).
    /// `Err(Hold)` = anything else; the file stays and we come back to it.
    fn upload(
        &self,
        batch: &IngestBatch,
        raw: bool,
    ) -> impl Future<Output = Result<u64, Hold>> + Send;
}

/// What one drain pass did — the numbers the tray and the lifetime counters read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    pub batches_uploaded: u64,
    pub events_uploaded: u64,
    pub segments_uploaded: u64,
    /// The WIRE would not take a batch, so the pass stopped early; whatever is
    /// left stays spooled.
    pub held: bool,
    /// Batches the server refused on their CONTENT this pass
    /// ([`HoldScope::Batch`]). They are still on disk and still retried — but
    /// they are not going to be accepted as written, so this is the number that
    /// means "someone has to look": a daemon/server contract mismatch.
    pub rejected: u64,
    /// What is still waiting after this pass.
    pub depth: SpoolDepth,
}

/// Progress hooks — the daemon feeds these into the live `Status`, tests ignore
/// them. Kept as a trait (rather than closures) so a caller cannot accidentally
/// report "sent" for something merely spooled: the only place these fire is after
/// a commit.
pub trait UploadObserver {
    /// A pass is starting with `batches` waiting. Fires before anything is on the
    /// wire, so this is what dates "uploading for 40s".
    fn on_pass_start(&mut self, batches: u64) {
        let _ = batches;
    }
    /// The server confirmed one batch.
    fn on_uploaded(&mut self, events: u64, segments: usize) {
        let _ = (events, segments);
    }
    /// The pass ended; `outcome.depth` is what remains.
    fn on_pass_end(&mut self, outcome: &DrainOutcome) {
        let _ = outcome;
    }
}
impl UploadObserver for () {}

/// Drain the spool once, oldest first, stopping at the first WIRE hold.
///
/// A batch the server refuses on its content ([`HoldScope::Batch`]) does NOT stop
/// the pass: it stays on disk and the queue behind it keeps moving. That
/// distinction is the difference between "one batch is stuck" and "this device
/// stopped reporting" — with a single shared stop condition, one unmodelled
/// record kind at the head of the spool held 98 batches for 14 hours.
///
/// Pure of timing and scheduling — the caller owns the backoff — so the decision
/// logic here is testable without sleeping.
pub async fn drain_once<U, O>(spool: &Spool, uploader: &U, observer: &mut O) -> DrainOutcome
where
    U: BatchUploader + Sync,
    O: UploadObserver,
{
    let mut outcome = DrainOutcome::default();
    let entries = match spool.list() {
        Ok(e) => e,
        Err(e) => {
            modelstat_log::log_error!("could not read the upload spool: {e}");
            outcome.held = true;
            return outcome;
        }
    };
    if entries.is_empty() {
        return outcome;
    }
    observer.on_pass_start(entries.len() as u64);

    // Each future owns ONE entry: load it, POST it, delete it on commit. Built
    // with a plain loop rather than `.map(|e| async move { … })` because a closure
    // returning a future that borrows its argument has to satisfy `FnOnce(&T)` for
    // every lifetime, which it cannot — the error surfaces far away as
    // "implementation of `Send` is not general enough".
    let mut futures = Vec::with_capacity(entries.len());
    for entry in &entries {
        futures.push(ship_one(spool, uploader, entry));
    }
    let mut inflight = futures_util::stream::iter(futures).buffer_unordered(UPLOAD_FANOUT);
    // The observer is touched only HERE, in the consumer, so it stays a plain
    // `&mut` even though the POSTs overlap.
    while let Some(result) = inflight.next().await {
        match result {
            Some(Ok(shipped)) => {
                outcome.batches_uploaded += 1;
                outcome.events_uploaded += shipped.accepted;
                outcome.segments_uploaded += shipped.segments as u64;
                observer.on_uploaded(shipped.accepted, shipped.segments);
            }
            // Nothing to send (vanished or quarantined — the spool logged it).
            None => continue,
            // The server rejected THIS payload and would reject it again; its
            // siblings are unaffected, so keep the pass going. The file stays put
            // (never dropped) and is retried on the next pass.
            Some(Err(Hold(HoldScope::Batch))) => {
                outcome.rejected += 1;
                continue;
            }
            // The wire itself is the problem — stop. Dropping the stream cancels
            // what is still in flight and starts nothing new. Everything left is
            // still on disk.
            Some(Err(Hold(HoldScope::Wire))) => {
                outcome.held = true;
                break;
            }
        }
    }
    drop(inflight);

    outcome.depth = spool.depth().unwrap_or_default();
    observer.on_pass_end(&outcome);
    outcome
}

/// One shipped batch's receipt.
struct Shipped {
    accepted: u64,
    segments: usize,
}

/// Load, POST and (on commit) delete one spooled batch. `None` = there was
/// nothing loadable to send.
async fn ship_one<U: BatchUploader>(
    spool: &Spool,
    uploader: &U,
    entry: &SpoolEntry,
) -> Option<Result<Shipped, Hold>> {
    let spooled = match spool.load(entry) {
        Ok(Some(b)) => b,
        Ok(None) => return None,
        Err(e) => {
            modelstat_log::log_error!(
                "could not read spooled batch {}: {e} — leaving it for the next pass",
                entry.path.display()
            );
            // Unreadable on OUR side, not refused by the server: treat it as the
            // wire being unavailable for this file and come back to it.
            return Some(Err(Hold(HoldScope::Wire)));
        }
    };
    match uploader.upload(&spooled.batch, spooled.raw).await {
        Ok(accepted) => {
            // Delete only now. A crash between the server's commit and this unlink
            // re-sends the batch next boot, which the server dedupes by id — the
            // safe direction to fail in.
            if let Err(e) = spool.remove(entry) {
                modelstat_log::log_error!(
                    "batch {} committed but its spool file could not be removed: {e} \
                     — it will be re-sent (and server-side deduped) until it can be",
                    spooled.batch.batch_id
                );
            }
            Some(Ok(Shipped {
                accepted,
                segments: spooled.segment_count,
            }))
        }
        Err(hold) => Some(Err(hold)),
    }
}

/// Drain until the spool is empty or a pass holds, and report what is left.
///
/// The ONE-SHOT counterpart to [`run_drain_loop`], for commands that scan in a
/// process which is about to exit (`modelstat sync` with no daemon running).
/// Without it those commands would redact, park, and leave — the data safe on
/// disk but going nowhere until a daemon happens to start.
///
/// It does NOT retry a hold: a person is waiting at a terminal, and the honest
/// answer to "the server is down" is to say so and leave the batches queued,
/// rather than block them for the length of an outage.
pub async fn drain_until_quiet<U, O>(spool: &Spool, uploader: &U, observer: &mut O) -> DrainOutcome
where
    U: BatchUploader + Sync,
    O: UploadObserver,
{
    let mut total = DrainOutcome::default();
    loop {
        let pass = drain_once(spool, uploader, observer).await;
        total.batches_uploaded += pass.batches_uploaded;
        total.events_uploaded += pass.events_uploaded;
        total.segments_uploaded += pass.segments_uploaded;
        total.held = pass.held;
        // Assigned, not accumulated: a refused batch is re-counted every pass, so
        // the LAST pass's number is the honest "how many is the server still
        // refusing" — summing would multiply one stuck batch by the pass count.
        total.rejected = pass.rejected;
        total.depth = pass.depth;
        // A pass that held, or that emptied the queue, or that could ship nothing
        // at all (every entry quarantined) — any of these means going round again
        // would just repeat itself.
        if pass.held || pass.depth.batches == 0 || pass.batches_uploaded == 0 {
            return total;
        }
    }
}

/// The next backoff after a held pass — doubling, capped, from a fixed floor.
pub fn next_backoff(current: Duration) -> Duration {
    if current.is_zero() {
        return BACKOFF_MIN;
    }
    (current * 2).min(BACKOFF_MAX)
}

/// Run the drain forever: work the queue, then park until something is spooled or
/// the backstop fires.
///
/// Never returns. The daemon stops it by aborting the task, which is safe at any
/// point: everything not yet confirmed is still on disk, and a batch aborted
/// between the server's commit and its `remove` is simply re-sent next boot and
/// deduped by id. There is deliberately no graceful-stop flag — it would only
/// delay shutdown to save a re-send the server already handles.
pub async fn run_drain_loop<U, O>(spool: Arc<Spool>, uploader: Arc<U>, mut observer: O)
where
    U: BatchUploader + Sync,
    O: UploadObserver,
{
    let wake = spool.wake();
    let mut backoff = Duration::ZERO;
    loop {
        let outcome = drain_once(&spool, &*uploader, &mut observer).await;
        if outcome.held {
            backoff = next_backoff(backoff);
            modelstat_log::log_warn!(
                "upload held with {} batch(es) still spooled ({} MB) — retrying in {}s; \
                 nothing is lost and no redaction is redone",
                outcome.depth.batches,
                outcome.depth.bytes / (1024 * 1024),
                backoff.as_secs()
            );
            tokio::time::sleep(backoff).await;
            continue;
        }
        // A clean pass earns a fresh start: the next blip begins at the floor
        // rather than inheriting an hour-old ceiling.
        backoff = Duration::ZERO;
        // Batches the server refuses by content stay queued, so `depth` alone no
        // longer means "there is work to retry immediately" — going straight round
        // on a pass that shipped NOTHING would spin the drain hot against a poison
        // batch. Say it loudly (this is a contract mismatch a human must fix) and
        // wait for a wake or the backstop, exactly like the idle case.
        if outcome.rejected > 0 && outcome.batches_uploaded == 0 {
            modelstat_log::log_error!(
                "the server REFUSED {} of {} queued batch(es) on their content — they \
                 stay on disk and nothing is lost, but they will not be accepted as \
                 written: this is a daemon/server contract mismatch. Please report it \
                 with the daemon version; `modelstat status` shows the count.",
                outcome.rejected,
                outcome.depth.batches
            );
        } else if outcome.depth.batches > 0 {
            continue; // more arrived while we worked — keep going
        }
        // Idle: sleep until a scan spools something, or re-check on the backstop
        // in case a notify was missed while we were mid-pass.
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(crate::spool::DRAIN_BACKSTOP) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_wire::{RawEvent, TokenUsage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("modelstat-drain-{}-{tag}", std::process::id()))
    }

    fn batch(id: &str, events: usize) -> IngestBatch {
        let event = |i: usize| RawEvent {
            seq: None,
            started_at: None,
            first_token_at: None,
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            source_event_id: format!("{id}-{i}"),
            ts: "2026-08-08T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s1".into(),
            actor_id: None,
            recipient_actor_id: None,
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: Some(TokenUsage::default()),
            tokens_unmapped: std::collections::BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        };
        IngestBatch {
            batch_id: id.into(),
            device_id: "dev_x".into(),
            daemon_version: "test".into(),
            events: (0..events).map(event).collect(),
            segments: Vec::new(),
            tool_calls: Vec::new(),
            session_installs: None,
            session_actors: None,
            session_titles: None,
            session_metadata: None,
            summarizer_mode: None,
            utc_offset_minutes: None,
            tz: None,
            tz_offset_minutes: None,
            redactor_mode: None,
            repo_anchors: None,
            segment_generations: None,
        }
    }

    fn spool(tag: &str) -> Spool {
        let dir = tmp_dir(tag);
        let _ = std::fs::remove_dir_all(&dir);
        Spool::open(dir, crate::spool::DEFAULT_MAX_SPOOL_BYTES).unwrap()
    }

    /// Commits everything.
    struct Accepting {
        seen: Mutex<Vec<String>>,
    }
    impl BatchUploader for Accepting {
        async fn upload(&self, batch: &IngestBatch, _raw: bool) -> Result<u64, Hold> {
            self.seen.lock().unwrap().push(batch.batch_id.clone());
            Ok(batch.events.len() as u64)
        }
    }

    /// Holds every time — the offline server.
    struct Offline {
        attempts: AtomicUsize,
    }
    impl BatchUploader for Offline {
        async fn upload(&self, _batch: &IngestBatch, _raw: bool) -> Result<u64, Hold> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(Hold(HoldScope::Wire))
        }
    }

    /// Refuses ONE named batch and commits the rest. Named rather than
    /// nth-attempt, so the test does not depend on how the runtime interleaves the
    /// overlapping POSTs.
    struct RefusesOne {
        refuse: &'static str,
        /// Peak concurrent uploads, to prove they really do overlap.
        inflight: AtomicUsize,
        peak: AtomicUsize,
    }
    impl RefusesOne {
        fn new(refuse: &'static str) -> Self {
            Self {
                refuse,
                inflight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }
        }
    }
    impl BatchUploader for RefusesOne {
        async fn upload(&self, batch: &IngestBatch, _raw: bool) -> Result<u64, Hold> {
            let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            // Suspend the way a real POST does while the server works. A sibling is
            // polled meanwhile — if the drain were sequential, `peak` could never
            // exceed one.
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            if batch.batch_id == self.refuse {
                return Err(Hold(HoldScope::Wire));
            }
            Ok(batch.events.len() as u64)
        }
    }

    #[tokio::test]
    async fn a_committed_batch_leaves_the_spool_and_is_counted() {
        let s = spool("commit");
        s.push(&batch("b1", 4), false, 2).unwrap();
        let up = Accepting {
            seen: Mutex::new(Vec::new()),
        };
        let out = drain_once(&s, &up, &mut ()).await;
        assert_eq!(out.batches_uploaded, 1);
        assert_eq!(out.events_uploaded, 4);
        assert_eq!(out.segments_uploaded, 2);
        assert!(!out.held);
        assert_eq!(out.depth.batches, 0, "a committed batch is deleted");
        let _ = std::fs::remove_dir_all(s.dir());
    }

    #[tokio::test]
    async fn a_held_batch_stays_on_disk_for_the_next_pass() {
        // THE regression this whole change exists for: an outage must not destroy
        // work. The batch is still there afterwards, ready to go, with no
        // re-redaction involved.
        let s = spool("hold");
        s.push(&batch("b1", 3), false, 0).unwrap();
        let up = Offline {
            attempts: AtomicUsize::new(0),
        };
        let out = drain_once(&s, &up, &mut ()).await;
        assert!(out.held);
        assert_eq!(out.batches_uploaded, 0);
        assert_eq!(out.depth.batches, 1, "the batch must survive the hold");

        // …and the very next pass, against a server that is back, ships it.
        let up = Accepting {
            seen: Mutex::new(Vec::new()),
        };
        let out = drain_once(&s, &up, &mut ()).await;
        assert_eq!(out.batches_uploaded, 1);
        assert_eq!(out.depth.batches, 0);
        let _ = std::fs::remove_dir_all(s.dir());
    }

    /// A refused batch survives; its siblings are free to land. The invariant that
    /// matters is per FILE — unlike the old scan-side fan-out, where one hold had
    /// to hold every cursor in the flush, each spooled batch is independent, so a
    /// sibling committing costs nothing.
    #[tokio::test]
    async fn a_refused_batch_survives_while_its_siblings_land() {
        let s = spool("refused");
        for i in 0..3 {
            s.push(&batch(&format!("b{i}"), 1), false, 0).unwrap();
        }
        let up = RefusesOne::new("b1");
        let out = drain_once(&s, &up, &mut ()).await;
        assert!(out.held, "the refusal must be reported, never swallowed");
        // b1 is still on disk, so the next pass retries exactly it.
        let left: Vec<String> = s
            .list()
            .unwrap()
            .iter()
            .filter_map(|e| s.load(e).unwrap())
            .map(|b| b.batch.batch_id)
            .collect();
        assert!(left.contains(&"b1".to_string()), "left: {left:?}");

        // A server that is back drains whatever remains — with no re-redaction,
        // because the batches were finished before they ever hit the disk.
        let up = Accepting {
            seen: Mutex::new(Vec::new()),
        };
        let out = drain_once(&s, &up, &mut ()).await;
        assert!(!out.held);
        assert_eq!(out.depth.batches, 0, "the queue drains completely");
        let _ = std::fs::remove_dir_all(s.dir());
    }

    /// Rejects a named batch on its CONTENT (`HoldScope::Batch`) and commits the
    /// rest — the shape of a server that will never accept one payload.
    struct RejectsOneByContent {
        reject: &'static str,
    }
    impl BatchUploader for RejectsOneByContent {
        async fn upload(&self, batch: &IngestBatch, _raw: bool) -> Result<u64, Hold> {
            if batch.batch_id == self.reject {
                return Err(Hold(HoldScope::Batch));
            }
            Ok(batch.events.len() as u64)
        }
    }

    /// THE incident, in one test: the OLDEST batch is one the server will never
    /// accept (an unmodelled `kind` it 400s on). Every batch behind it must still
    /// ship in the SAME pass.
    ///
    /// Before the scope split this was a total, silent outage — the pass stopped at
    /// the head of the queue, so 98 batches sat spooled for 14 hours while the
    /// daemon reported "scanning" and `segments_sent: 0`. The queue is deliberately
    /// longer than `UPLOAD_FANOUT` so a pass that merely finishes its first
    /// concurrent window cannot pass this test.
    #[tokio::test]
    async fn a_content_rejected_batch_does_not_block_the_queue_behind_it() {
        let s = spool("rejected-head");
        let total = UPLOAD_FANOUT * 3 + 1;
        // b0 is oldest, and is the poison one.
        for i in 0..total {
            s.push(&batch(&format!("b{i}"), 1), false, 0).unwrap();
        }
        let up = RejectsOneByContent { reject: "b0" };
        let out = drain_once(&s, &up, &mut ()).await;

        assert_eq!(
            out.batches_uploaded as usize,
            total - 1,
            "every batch behind the rejected one must ship in this same pass"
        );
        assert_eq!(out.rejected, 1, "the refusal is counted, never swallowed");
        assert!(
            !out.held,
            "a content rejection is not an outage — it must not stop the pass"
        );

        // Never dropped: exactly the poison batch is still on disk, ready to be
        // retried (and to succeed the moment the server understands it).
        let left: Vec<String> = s
            .list()
            .unwrap()
            .iter()
            .filter_map(|e| s.load(e).unwrap())
            .map(|b| b.batch.batch_id)
            .collect();
        assert_eq!(left, vec!["b0".to_string()], "left: {left:?}");

        // And once the server accepts it, the queue is empty — no re-redaction.
        let up = Accepting {
            seen: Mutex::new(Vec::new()),
        };
        let out = drain_once(&s, &up, &mut ()).await;
        assert_eq!(out.batches_uploaded, 1);
        assert_eq!(out.rejected, 0);
        assert_eq!(out.depth.batches, 0);
        let _ = std::fs::remove_dir_all(s.dir());
    }

    /// A backlog must not go out one POST at a time. Each upload spends its life
    /// waiting on the server, so a sequential drain would make a first-run backfill
    /// take hours of mostly-idle wire.
    #[tokio::test]
    async fn uploads_overlap_instead_of_queueing() {
        let s = spool("overlap");
        for i in 0..4 {
            s.push(&batch(&format!("b{i}"), 1), false, 0).unwrap();
        }
        let up = RefusesOne::new("none-of-them");
        let out = drain_once(&s, &up, &mut ()).await;
        assert_eq!(out.batches_uploaded, 4);
        assert!(
            up.peak.load(Ordering::SeqCst) > 1,
            "uploads must overlap; saw peak in-flight of {}",
            up.peak.load(Ordering::SeqCst)
        );
        let _ = std::fs::remove_dir_all(s.dir());
    }

    /// Oldest first — not load-bearing (ids are deterministic and the server
    /// upserts) but it is what a user watching a backlog drain expects to see.
    #[tokio::test]
    async fn the_queue_is_started_oldest_first() {
        let s = spool("order");
        for i in 0..3 {
            s.push(&batch(&format!("b{i}"), 1), false, 0).unwrap();
        }
        let up = Accepting {
            seen: Mutex::new(Vec::new()),
        };
        let out = drain_once(&s, &up, &mut ()).await;
        assert_eq!(out.batches_uploaded, 3);
        assert_eq!(*up.seen.lock().unwrap(), vec!["b0", "b1", "b2"]);
        let _ = std::fs::remove_dir_all(s.dir());
    }

    #[tokio::test]
    async fn an_empty_spool_is_a_no_op() {
        let s = spool("empty");
        let up = Accepting {
            seen: Mutex::new(Vec::new()),
        };
        let out = drain_once(&s, &up, &mut ()).await;
        assert_eq!(out, DrainOutcome::default());
        assert!(!out.held);
        let _ = std::fs::remove_dir_all(s.dir());
    }

    #[tokio::test]
    async fn a_quarantined_batch_does_not_block_the_ones_behind_it() {
        let s = spool("quarantine");
        std::fs::write(s.dir().join("00000000000000000000.json"), b"{ broken").unwrap();
        s.push(&batch("good", 1), false, 0).unwrap();
        let up = Accepting {
            seen: Mutex::new(Vec::new()),
        };
        let out = drain_once(&s, &up, &mut ()).await;
        assert_eq!(out.batches_uploaded, 1, "the readable one still ships");
        assert_eq!(*up.seen.lock().unwrap(), vec!["good"]);
        assert!(!out.held);
        let _ = std::fs::remove_dir_all(s.dir());
    }

    #[test]
    fn backoff_climbs_from_the_floor_and_stops_at_the_ceiling() {
        let mut d = Duration::ZERO;
        d = next_backoff(d);
        assert_eq!(d, BACKOFF_MIN);
        for _ in 0..20 {
            d = next_backoff(d);
        }
        assert_eq!(
            d, BACKOFF_MAX,
            "an all-night outage must not back off forever"
        );
    }
}
