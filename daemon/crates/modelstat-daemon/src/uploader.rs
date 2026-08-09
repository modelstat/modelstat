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

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;

use crate::spool::{Spool, SpoolDepth, SpoolEntry};
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold;

/// Ships one batch. Backed by `DeviceApi` in the daemon; a fake in tests.
pub trait BatchUploader {
    /// `Ok(accepted)` = committed (the spooled file may be deleted).
    /// `Err(Hold)` = anything else; the file stays and we come back to it.
    fn upload(
        &self,
        batch: &IngestBatch,
        raw: bool,
    ) -> impl std::future::Future<Output = Result<u64, Hold>> + Send;
}

/// What one drain pass did — the numbers the tray and the lifetime counters read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    pub batches_uploaded: u64,
    pub events_uploaded: u64,
    pub segments_uploaded: u64,
    /// The server would not take a batch; whatever is left stays spooled.
    pub held: bool,
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

/// Drain the spool once, oldest first, stopping at the first hold.
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
            // Stop: dropping the stream cancels what is still in flight and starts
            // nothing new. Everything left is still on disk.
            Some(Err(Hold)) => {
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
            return Some(Err(Hold));
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
        Err(Hold) => Some(Err(Hold)),
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
        if outcome.depth.batches > 0 {
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
            content_bytes: None,
            source_event_id: format!("{id}-{i}"),
            ts: "2026-08-08T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s1".into(),
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
            session_titles: None,
            session_metadata: None,
            summarizer_mode: None,
            redactor_mode: None,
            repo_anchors: None,
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
            Err(Hold)
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
                return Err(Hold);
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
