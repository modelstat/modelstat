//! The scan loop — a port of `apps/daemon/src/scan.ts`'s `runScanOverJobs`
//! (the shared engine behind both `scanAll` and `scanSession`). Given an ordered
//! job list it parses each transcript, buffers the events, composes batches via
//! [`build_flush_batches`], ships them, and advances each file's cursor **only
//! after a confirmed upload** — so a mid-scan failure re-parses the same files
//! next cycle and retries (idempotent server-side). Good data is never dropped.
//!
//! Everything the loop does I/O with is an injected seam (`parse`, `checksum`,
//! `correct_events`, `enrich_scripts`, the [`BatchUploader`], the
//! [`CursorStore`]) so the whole state machine is unit-testable against a fake
//! engine + fake server without touching a real `.jsonl` or the network. The
//! daemon-main wires the real adapters (the [`crate::flush`] doc names them).

use std::collections::BTreeMap;

use modelstat_ingest::state::FileCursor;
use modelstat_parsers::{GitEnrichment, ParseResult, ToolCallDraft};
use modelstat_pipeline::{Embedder, LinkExtractor, ResilientSummarizer, Summarizer};
use modelstat_redact::NerModel;
use modelstat_wire::{IngestBatch, RawEvent, Segment};

use crate::discover_jobs::ScanJob;
use crate::flush::{build_flush_batches, FlushOutcome};

/// Ship a batch the moment the event buffer reaches this many events — mirrors
/// `INGEST_BATCH_MAX_EVENTS` (daemon-core/config). Because the check runs after
/// every single push, batches are exactly this size except the trailing one, so
/// memory stays near one batch even on a full re-scan of months of history.
pub const BATCH_MAX_EVENTS: usize = 1_000;
/// Flush when the tool-call buffer reaches this — a second (rare) flush trigger
/// so a pathological run of calls can't grow the buffer unbounded.
pub const BATCH_MAX_TOOL_CALLS: usize = 20_000;
/// Regression backstop: with per-push flushing the buffer can never reach this,
/// so tripping it means incremental flushing has broken (see the `debug_assert`).
pub const BATCH_BUFFER_HARD_CAP: usize = BATCH_MAX_EVENTS * 2;
/// Cold-start bound: process at most this many CHANGED files per incremental
/// scan. Newest-first + this cap means a session you just finished lands fast
/// while a big backlog drains over quick follow-up cycles (`more_pending`).
pub const MAX_FILES_PER_SCAN: usize = 12;
/// How many event chunks (≤256 events each) the streaming parser may run ahead of
/// the buffer/flush loop. Bounds the in-flight memory to this × chunk + one batch,
/// so even a multi-hundred-MB transcript never fully materialises. Backpressure:
/// the (blocking-thread) parser parks on `blocking_send` once this many chunks
/// queue, so it stops reading while a batch is being summarised + uploaded.
const STREAM_CHANNEL_CAP: usize = 4;

/// Per-cycle scan bounds. `scan_all` passes `{ Some(MAX_FILES_PER_SCAN), false }`;
/// the eager single-session force-scan passes `{ None, true }`.
#[derive(Debug, Clone, Copy)]
pub struct RunScanOptions {
    /// Cap on CHANGED files per call; `None` = uncapped (single-session).
    pub max_files: Option<usize>,
    /// Skip the per-file unchanged-guard and re-read every job (the cursor is
    /// still advanced after a confirmed upload, so incremental stays correct).
    pub force_read_all: bool,
}

/// What one scan pass accomplished. Mirrors TS `ScanResult`, plus `held` — the
/// Rust daemon HOLDS (never-drop, retry next cycle) instead of throwing when the
/// engine is down or the upload can't commit (see [`build_flush_batches`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanTallies {
    pub files_scanned: usize,
    pub files_unchanged: usize,
    pub batches_uploaded: u64,
    pub events_uploaded: u64,
    pub segments_uploaded: u64,
    /// The per-cycle file cap was hit with files still pending — the caller
    /// should re-scan promptly to drain the rest (newest-first).
    pub more_pending: bool,
    /// A flush was HELD (engine down / offline / non-commit): the buffered files'
    /// cursors were left un-advanced and will be retried next cycle.
    pub held: bool,
}

/// Where per-file scan cursors live. The daemon backs this with `RuntimeState`
/// (persisted to `state.json`); tests use an in-memory store. A cursor advances
/// ONLY after the batch carrying its file's events is confirmed uploaded. Port
/// of TS `state.getCursor` / `state.setCursor`.
pub trait CursorStore {
    fn get_cursor(&self, path: &str) -> Option<FileCursor>;
    fn set_cursor(&mut self, path: &str, cursor: FileCursor);
}

impl CursorStore for modelstat_ingest::RuntimeState {
    fn get_cursor(&self, path: &str) -> Option<FileCursor> {
        self.cursor.get(path).cloned()
    }
    fn set_cursor(&mut self, path: &str, cursor: FileCursor) {
        self.cursor.insert(path.to_string(), cursor);
    }
}

/// The hold signal: a flush could not commit (engine down, offline, or a
/// non-2xx the never-drop matrix retries). The whole flush is held — cursors are
/// NOT advanced, so the same events re-ship next cycle (idempotent server-side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold;

/// Ships one assembled batch. The daemon backs this with `DeviceApi::upload_batch`
/// (the M4 Part-1 never-drop matrix); tests use a fake that records or holds.
/// `Ok(accepted)` = a CONFIRMED commit (cursor may advance); `Err(Hold)` = any
/// non-commit (no token / reauth failed / retries exhausted / offline).
pub trait BatchUploader {
    async fn upload(&mut self, batch: &IngestBatch, raw: bool) -> Result<u64, Hold>;
}

/// Progress sink for a scan — the daemon updates `last-status.json` + the tray
/// from these; tests pass `&mut ()`. Port of TS `ScanCallbacks` (minus the
/// per-segment `onProgress`, which the pure batch builder doesn't surface).
pub trait ScanObserver {
    /// Before parsing each file — `index` 0-based, `total` = files discovered.
    fn on_file(&mut self, path: &str, index: usize, total: usize) {
        let _ = (path, index, total);
    }
    /// Right before a batch POSTs — these records are now in-flight.
    fn on_upload(&mut self, events: usize, segments: usize) {
        let _ = (events, segments);
    }
    /// After the POST is confirmed — `events` = server-accepted count.
    fn on_uploaded(&mut self, events: usize, segments: usize) {
        let _ = (events, segments);
    }
}
impl ScanObserver for () {}

/// Assemble + ship whatever is buffered, then (on full success) advance the
/// buffered files' cursors. Empties `buffer`/`tool_buffer`/`pending_cursors` on a
/// confirmed commit; on `Err(Hold)` the caller stops the scan and the un-advanced
/// files retry next cycle. Shared by the mid-scan (buffer-full) and trailing
/// flushes. Port of the `flushBatch` + `commitBatch` closures.
#[allow(clippy::too_many_arguments)]
async fn flush_buffer<S, E, N, G, U, CE>(
    device_id: &str,
    daemon_version: &str,
    mode: &str,
    buffer: &mut Vec<RawEvent>,
    tool_buffer: &mut Vec<ToolCallDraft>,
    pending_cursors: &mut Vec<(String, FileCursor)>,
    run_segments: &mut BTreeMap<String, Vec<Segment>>,
    resilient: &ResilientSummarizer<S>,
    embedder: &E,
    ner: &N,
    git: &mut G,
    extract_links: Option<&LinkExtractor<'_>>,
    correct_events: &mut CE,
    uploader: &mut U,
    cursors: &mut (dyn CursorStore + Send),
    observer: &mut (dyn ScanObserver + Send),
    tallies: &mut ScanTallies,
) -> Result<(), Hold>
where
    S: Summarizer,
    E: Embedder,
    N: NerModel,
    G: GitEnrichment + Send,
    U: BatchUploader,
    CE: FnMut(Vec<RawEvent>) -> Vec<RawEvent>,
{
    if buffer.is_empty() && tool_buffer.is_empty() {
        return Ok(());
    }
    // Correct each event's repo identity to the AUTHORITATIVE on-disk git remote
    // BEFORE segmentation (so `projects` keys on the real owner/repo, not a
    // path-guessed subdir). Token normalisation happens inside build_flush_batches;
    // the two passes touch disjoint fields, so order is irrelevant.
    let events = correct_events(std::mem::take(buffer));
    let drafts = std::mem::take(tool_buffer);
    let outcome = build_flush_batches(
        device_id,
        daemon_version,
        mode,
        events,
        drafts,
        resilient,
        embedder,
        ner,
        // Fresh reborrow per flush — `git` outlives every flush, so this dodges
        // the reborrow-across-await lifetime trap (see flush.rs LESSON). `+ Send`
        // so the erased trait object stays `Send` for the spawned scan task.
        Some(&mut *git as &mut (dyn GitEnrichment + Send)),
        extract_links,
        run_segments,
    )
    .await;
    let batches = match outcome {
        // Engine down / cloud NER unavailable → hold the whole flush, never a
        // partial batch (that would advance the cursor past un-summarised events).
        FlushOutcome::Held => return Err(Hold),
        FlushOutcome::Ready(b) => b,
    };
    for pb in &batches {
        observer.on_upload(pb.batch.events.len(), pb.segment_count);
        // A non-commit HOLDS the whole flush: we never reach the cursor-advance
        // below, so the next scan re-parses the same files and retries.
        let accepted = uploader.upload(&pb.batch, pb.raw).await?;
        tallies.batches_uploaded += 1;
        tallies.events_uploaded += accepted;
        tallies.segments_uploaded += pb.segment_count as u64;
        observer.on_uploaded(accepted as usize, pb.segment_count);
    }
    // Every batch in this flush committed — persist the buffered files' cursors
    // once, atomically. Upserts are server-side idempotent, so a crash between
    // here and the next scan just re-sends the same events safely.
    for (path, cursor) in pending_cursors.drain(..) {
        cursors.set_cursor(&path, cursor);
    }
    Ok(())
}

/// The shared parse → summarise → upload loop over an ordered job list. Holds at
/// most ~one batch in memory and advances each file's cursor only after its
/// events land, so a mid-scan failure re-tries the same events next run. Both the
/// incremental `scan_all` and the eager single-session scan funnel through here;
/// their only differences are [`RunScanOptions`]. Port of `runScanOverJobs`.
///
/// The many parameters are the seams that let this be tested without real I/O:
/// `parse`/`checksum` read a job (daemon wires `parse_job`/`quick_checksum`),
/// `correct_events` applies authoritative-git correction (daemon wires
/// `resolve_authoritative_git` over its own resolver — kept separate from `git`
/// so two callers don't double-borrow one resolver), `exists`/`read_file` probe +
/// read referenced script files for the best-effort script-summary enrichment
/// (daemon wires the real `Path::exists` + a capped file read), `uploader`/
/// `cursors`/`observer` are the sinks. `git` is the metadata enrichment the loop
/// OWNS and lends to each flush.
#[allow(clippy::too_many_arguments)]
pub async fn run_scan_over_jobs<S, E, N, G, U, P, C, CE>(
    ordered: Vec<ScanJob>,
    device_id: &str,
    daemon_version: &str,
    mode: &str,
    opts: RunScanOptions,
    resilient: &ResilientSummarizer<S>,
    embedder: &E,
    ner: &N,
    git: &mut G,
    extract_links: Option<&LinkExtractor<'_>>,
    parse: P,
    checksum: C,
    mut correct_events: CE,
    // `+ Sync` so `&exists`/`&read_file` are `Send` — the scan runs inside the
    // daemon's tokio-spawned single-flight task, whose future must be `Send`.
    exists: &(dyn Fn(&str) -> bool + Sync),
    read_file: &(dyn Fn(&str) -> Option<String> + Sync),
    uploader: &mut U,
    // `+ Send` on the trait objects: the scan runs in the daemon's tokio-spawned
    // single-flight task, whose future must be `Send` — and a `dyn Trait` erases
    // the concrete type's Send-ness unless the bound is spelled out. Every real
    // impl (RuntimeState, StatusObserver, RealGitEnrichment) is `Send`.
    cursors: &mut (dyn CursorStore + Send),
    observer: &mut (dyn ScanObserver + Send),
) -> ScanTallies
where
    S: Summarizer,
    E: Embedder,
    N: NerModel,
    G: GitEnrichment + Send,
    U: BatchUploader,
    // The streaming parse seam: emits events in bounded chunks (never a full
    // materialised `ParseResult.events`) + returns the tool-call/stats tail. `Fn +
    // Send + Sync + Clone + 'static` so the loop can run it on a `spawn_blocking`
    // thread (a fresh clone per file) — a sync parser can't await the async flush,
    // so the bounded channel is what carries backpressure between them.
    P: Fn(&ScanJob, &mut dyn FnMut(Vec<RawEvent>)) -> std::io::Result<ParseResult>
        + Send
        + Sync
        + Clone
        + 'static,
    C: Fn(&str) -> Option<FileCursor>,
    CE: FnMut(Vec<RawEvent>) -> Vec<RawEvent>,
{
    let mut tallies = ScanTallies::default();
    // The current batch's events + the tool-call drafts travelling with them.
    let mut buffer: Vec<RawEvent> = Vec::new();
    let mut tool_buffer: Vec<ToolCallDraft> = Vec::new();
    // Files whose events are buffered but whose cursor has NOT advanced yet —
    // advanced only when the batch carrying their events commits (never-drop).
    let mut pending_cursors: Vec<(String, FileCursor)> = Vec::new();
    // Segments accumulated per session across the WHOLE run: a big file can split
    // a session across flush boundaries, and its title / call-attribution must
    // read every segment seen so far (else a later partial-view title wins).
    let mut run_segments: BTreeMap<String, Vec<Segment>> = BTreeMap::new();

    // `flush!()` ships + clears the buffers and advances cursors on success;
    // `Err(Hold)` means the caller must stop (engine down / offline). Each
    // expansion reborrows the shared state for exactly one flush.
    macro_rules! flush {
        () => {
            flush_buffer(
                device_id,
                daemon_version,
                mode,
                &mut buffer,
                &mut tool_buffer,
                &mut pending_cursors,
                &mut run_segments,
                resilient,
                embedder,
                ner,
                &mut *git,
                extract_links,
                &mut correct_events,
                &mut *uploader,
                &mut *cursors,
                &mut *observer,
                &mut tallies,
            )
            .await
        };
    }

    let total = ordered.len();
    for (i, job) in ordered.iter().enumerate() {
        observer.on_file(&job.path, i, total);
        let cur = cursors.get_cursor(&job.path);
        let cs = checksum(&job.path);
        // Incremental scans skip a file whose size + tail are unchanged since the
        // last upload (NOT mtime — a touch that didn't change bytes re-uses the
        // cursor). force_read_all bypasses this so a targeted session re-uploads.
        if !opts.force_read_all {
            if let (Some(cs), Some(cur)) = (cs.as_ref(), cur.as_ref()) {
                if cur.size == cs.size && cur.tail_hash == cs.tail_hash {
                    tallies.files_unchanged += 1;
                    continue;
                }
            }
        }
        tallies.files_scanned += 1;
        // Stream this file's events through a bounded channel while the SYNC
        // streaming parser runs on a spawn-blocking thread. The parser can't await
        // the async flush, so the channel bridges them: the async side drains a
        // chunk, buffers it, and flushes the moment a full batch accumulates; the
        // parser parks on a full channel (backpressure) so it never reads ahead of
        // the flush — memory stays near one batch + the channel even mid-file, so
        // a multi-hundred-MB transcript never fully materialises. A held flush
        // stops the scan — the file's cursor was never queued (that happens
        // post-parse below), so it re-parses whole next cycle.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<RawEvent>>(STREAM_CHANNEL_CAP);
        let parse_one = parse.clone();
        let job_owned = job.clone();
        let parse_handle = tokio::task::spawn_blocking(move || {
            let mut emit = move |chunk: Vec<RawEvent>| {
                // A closed channel (the async side held + returned) just drops the
                // remainder — the cursor never advanced, so the file re-parses.
                let _ = tx.blocking_send(chunk);
            };
            parse_one(&job_owned, &mut emit)
        });
        let mut held = false;
        while let Some(chunk) = rx.recv().await {
            for e in chunk {
                buffer.push(e);
                if buffer.len() >= BATCH_MAX_EVENTS && flush!().is_err() {
                    held = true;
                    break;
                }
            }
            if held {
                break;
            }
        }
        if held {
            tallies.held = true;
            return tallies;
        }
        debug_assert!(
            buffer.len() <= BATCH_BUFFER_HARD_CAP,
            "scan event buffer exceeded {BATCH_BUFFER_HARD_CAP} events — \
             incremental batch flushing has regressed"
        );
        // The parser finished streaming; take its tail (tool-call drafts + script
        // contexts + stats). A parse error / task panic skips this one file loudly
        // — the scan carries on and the file re-tries next cycle.
        let mut r = match parse_handle.await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                eprintln!("modelstat: parse failed for {}: {e}", job.path);
                continue;
            }
            Err(join) => {
                eprintln!("modelstat: parse task panicked for {}: {join}", job.path);
                continue;
            }
        };
        // Summarise each command's script/bash FILES into the drafts' redacted
        // abstracts — best-effort + additive; a failure leaves them empty and never
        // blocks the upload. Runs before the drafts buffer so the enriched version
        // ships. Uses the RAW engine (`resilient.engine()`), NOT the resilient
        // wrapper: script abstracts are additive, so an engine-down failure must
        // simply omit the script — never trip the hold-and-retry that the mandatory
        // summarise path (build_flush_batches) owns.
        crate::enrich_scripts::enrich_tool_call_scripts(
            &mut r.tool_calls,
            &r.script_contexts,
            resilient.engine(),
            exists,
            read_file,
            Some(ner),
        )
        .await;
        for c in r.tool_calls.drain(..) {
            if tool_buffer.len() >= BATCH_MAX_TOOL_CALLS && flush!().is_err() {
                tallies.held = true;
                return tallies;
            }
            tool_buffer.push(c);
        }
        // Queue the cursor advance — applied by the next successful flush, not
        // before. If we're offline the next scan retries from the same position. A
        // checksum read that failed leaves no cursor (the file re-parses next
        // time), matching the TS `.catch(() => null)`.
        if let Some(cs) = cs {
            pending_cursors.push((job.path.clone(), cs));
        }
        // Stop after the cap so memory + time per cycle stay bounded. The trailing
        // flush uploads what's buffered; `more_pending` tells the daemon to
        // re-scan the next newest batch. A null cap never breaks early.
        if let Some(max) = opts.max_files {
            if tallies.files_scanned >= max {
                tallies.more_pending = true;
                break;
            }
        }
    }
    // Ship the trailing partial batch. A hold here just leaves it for next cycle.
    if flush!().is_err() {
        tallies.held = true;
    }
    tallies
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover_jobs::ParserKind;
    use modelstat_ingest::RuntimeState;
    use modelstat_parsers::{FileChange, ParseStats, PrOutcome};
    use modelstat_pipeline::NoEmbedder;
    use modelstat_redact::UnavailableNer;
    use modelstat_sumclient::{CompleteRequest, SumError};
    use modelstat_wire::GitContext;
    use std::time::Duration;

    // The scan's script-enrichment file seams. Every test here parses events with
    // NO script contexts (parse_with sets `script_contexts` empty), so
    // enrich_tool_call_scripts returns early and these are never actually called —
    // they exist only to satisfy the two seam parameters.
    fn no_exists(_path: &str) -> bool {
        false
    }
    fn no_read(_path: &str) -> Option<String> {
        None
    }

    // A fake engine: healthy → a fixed reply for every pass; failing → 503 so the
    // resilient wrapper reports Held.
    struct Fake {
        reply: String,
        failing: bool,
    }
    impl Summarizer for Fake {
        async fn complete(&self, _req: &CompleteRequest) -> Result<String, SumError> {
            if self.failing {
                Err(SumError::Http(503))
            } else {
                Ok(self.reply.clone())
            }
        }
    }

    // Metadata git that knows nothing — metadata builds from content channels.
    struct NoGit;
    impl GitEnrichment for NoGit {
        fn resolve_git(&mut self, _cwd: Option<&str>) -> Option<GitContext> {
            None
        }
        fn check_pr_outcome(&mut self, _cwd: &str, _pr: u64) -> Option<PrOutcome> {
            None
        }
        fn collect_files_changed(
            &mut self,
            _cwd: &str,
            _since: &str,
            _until: &str,
        ) -> Option<Vec<FileChange>> {
            None
        }
    }

    // Records every committed batch, or holds every upload when `hold` is set.
    #[derive(Default)]
    struct RecordingUploader {
        /// (event_count, raw) per committed batch.
        uploaded: Vec<(usize, bool)>,
        hold: bool,
    }
    impl BatchUploader for RecordingUploader {
        async fn upload(&mut self, batch: &IngestBatch, raw: bool) -> Result<u64, Hold> {
            if self.hold {
                return Err(Hold);
            }
            let n = batch.events.len();
            self.uploaded.push((n, raw));
            Ok(n as u64)
        }
    }

    fn healthy() -> ResilientSummarizer<Fake> {
        ResilientSummarizer::with_cooldown(
            Fake {
                reply: "Did the thing".into(),
                failing: false,
            },
            Duration::ZERO,
        )
    }

    fn ev(session: &str, ts: &str) -> RawEvent {
        RawEvent {
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
            pricing_mode: None,
        }
    }

    fn job(path: &str) -> ScanJob {
        ScanJob {
            path: path.into(),
            kind: ParserKind::ClaudeCode,
        }
    }

    // A streaming parse seam that emits the given events (as ONE chunk) for every
    // job, returning an empty-`events` tail (mirrors the real streaming parsers).
    fn parse_with(
        events: Vec<RawEvent>,
    ) -> impl Fn(&ScanJob, &mut dyn FnMut(Vec<RawEvent>)) -> std::io::Result<ParseResult>
           + Send
           + Sync
           + Clone
           + 'static {
        move |j: &ScanJob, emit: &mut dyn FnMut(Vec<RawEvent>)| {
            if !events.is_empty() {
                emit(events.clone());
            }
            Ok(ParseResult {
                events: Vec::new(),
                tool_calls: Vec::new(),
                script_contexts: Vec::new(),
                stats: ParseStats::default(),
                source_file: j.path.clone(),
            })
        }
    }

    // A streaming parse seam that emits events in MULTIPLE chunks of `chunk_size`
    // — exercises the spawn-blocking + bounded-channel bridge across chunk
    // boundaries (the memory-ceiling path a single huge transcript takes).
    fn parse_in_chunks(
        events: Vec<RawEvent>,
        chunk_size: usize,
    ) -> impl Fn(&ScanJob, &mut dyn FnMut(Vec<RawEvent>)) -> std::io::Result<ParseResult>
           + Send
           + Sync
           + Clone
           + 'static {
        move |j: &ScanJob, emit: &mut dyn FnMut(Vec<RawEvent>)| {
            for chunk in events.chunks(chunk_size.max(1)) {
                emit(chunk.to_vec());
            }
            Ok(ParseResult {
                events: Vec::new(),
                tool_calls: Vec::new(),
                script_contexts: Vec::new(),
                stats: ParseStats::default(),
                source_file: j.path.clone(),
            })
        }
    }

    // A checksum seam: distinct size+tail per path so files look "changed".
    fn checksum(path: &str) -> Option<FileCursor> {
        Some(FileCursor {
            size: 100,
            mtime: 1,
            tail_hash: format!("h-{path}"),
        })
    }

    fn opts(max: Option<usize>, force: bool) -> RunScanOptions {
        RunScanOptions {
            max_files: max,
            force_read_all: force,
        }
    }

    #[tokio::test]
    async fn scans_files_uploads_and_advances_cursors() {
        let resilient = healthy();
        let mut git = NoGit;
        let mut uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let events = vec![
            ev("s1", "2026-07-16T10:00:00.000Z"),
            ev("s1", "2026-07-16T10:01:00.000Z"),
        ];
        let t = run_scan_over_jobs(
            vec![job("/a.jsonl"), job("/b.jsonl")],
            "dev1",
            "9.9.9",
            "local",
            opts(Some(12), false),
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &mut uploader,
            &mut cursors,
            &mut obs,
        )
        .await;

        assert_eq!(t.files_scanned, 2);
        assert_eq!(t.files_unchanged, 0);
        assert!(!t.held);
        assert!(t.batches_uploaded >= 1);
        // Local mode never ships raw.
        assert!(uploader.uploaded.iter().all(|(_, raw)| !raw));
        // Both files' cursors advanced only after their events committed.
        assert!(cursors.get_cursor("/a.jsonl").is_some());
        assert!(cursors.get_cursor("/b.jsonl").is_some());
    }

    #[tokio::test]
    async fn skips_a_file_whose_size_and_tail_are_unchanged() {
        let resilient = healthy();
        let mut git = NoGit;
        let mut uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        // Pre-seed the cursor to EXACTLY what checksum() will report for /a.jsonl.
        cursors.set_cursor(
            "/a.jsonl",
            FileCursor {
                size: 100,
                mtime: 999, // mtime differs — must NOT matter
                tail_hash: "h-/a.jsonl".into(),
            },
        );
        let t = run_scan_over_jobs(
            vec![job("/a.jsonl")],
            "dev1",
            "9.9.9",
            "local",
            opts(Some(12), false),
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            &mut git,
            None,
            parse_with(vec![ev("s1", "2026-07-16T10:00:00.000Z")]),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &mut uploader,
            &mut cursors,
            &mut obs,
        )
        .await;

        assert_eq!(t.files_unchanged, 1);
        assert_eq!(t.files_scanned, 0);
        assert_eq!(t.batches_uploaded, 0);
        assert!(uploader.uploaded.is_empty());
    }

    #[tokio::test]
    async fn force_read_all_bypasses_the_unchanged_skip() {
        let resilient = healthy();
        let mut git = NoGit;
        let mut uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        cursors.set_cursor(
            "/a.jsonl",
            FileCursor {
                size: 100,
                mtime: 1,
                tail_hash: "h-/a.jsonl".into(),
            },
        );
        let t = run_scan_over_jobs(
            vec![job("/a.jsonl")],
            "dev1",
            "9.9.9",
            "local",
            opts(None, true), // force_read_all
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            &mut git,
            None,
            parse_with(vec![ev("s1", "2026-07-16T10:00:00.000Z")]),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &mut uploader,
            &mut cursors,
            &mut obs,
        )
        .await;

        assert_eq!(t.files_scanned, 1);
        assert_eq!(t.files_unchanged, 0);
        assert!(t.batches_uploaded >= 1);
    }

    #[tokio::test]
    async fn engine_down_holds_and_never_advances_cursors() {
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: String::new(),
                failing: true,
            },
            Duration::ZERO,
        );
        let mut git = NoGit;
        let mut uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let t = run_scan_over_jobs(
            vec![job("/a.jsonl")],
            "dev1",
            "9.9.9",
            "local",
            opts(Some(12), false),
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            &mut git,
            None,
            parse_with(vec![ev("s1", "2026-07-16T10:00:00.000Z")]),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &mut uploader,
            &mut cursors,
            &mut obs,
        )
        .await;

        assert!(t.held);
        assert_eq!(t.batches_uploaded, 0);
        // Never-drop: the file's cursor stays put so it re-parses next cycle.
        assert!(cursors.get_cursor("/a.jsonl").is_none());
        assert!(uploader.uploaded.is_empty());
    }

    #[tokio::test]
    async fn upload_hold_leaves_cursors_un_advanced() {
        let resilient = healthy();
        let mut git = NoGit;
        // Engine is healthy (batches build), but every upload holds.
        let mut uploader = RecordingUploader {
            hold: true,
            ..Default::default()
        };
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let t = run_scan_over_jobs(
            vec![job("/a.jsonl")],
            "dev1",
            "9.9.9",
            "local",
            opts(Some(12), false),
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            &mut git,
            None,
            parse_with(vec![ev("s1", "2026-07-16T10:00:00.000Z")]),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &mut uploader,
            &mut cursors,
            &mut obs,
        )
        .await;

        assert!(t.held);
        assert_eq!(t.batches_uploaded, 0);
        assert!(cursors.get_cursor("/a.jsonl").is_none());
    }

    #[tokio::test]
    async fn file_cap_stops_early_and_sets_more_pending() {
        let resilient = healthy();
        let mut git = NoGit;
        let mut uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let t = run_scan_over_jobs(
            vec![job("/a.jsonl"), job("/b.jsonl")],
            "dev1",
            "9.9.9",
            "local",
            opts(Some(1), false), // cap at ONE changed file
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            &mut git,
            None,
            parse_with(vec![ev("s1", "2026-07-16T10:00:00.000Z")]),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &mut uploader,
            &mut cursors,
            &mut obs,
        )
        .await;

        assert_eq!(t.files_scanned, 1);
        assert!(t.more_pending);
        // Only the first file's events shipped + cursor advanced.
        assert!(cursors.get_cursor("/a.jsonl").is_some());
        assert!(cursors.get_cursor("/b.jsonl").is_none());
    }

    #[tokio::test]
    async fn a_session_straddling_the_flush_boundary_advances_the_cursor_once() {
        // 1100 events in one file → the buffer hits BATCH_MAX_EVENTS mid-file and
        // ships a first batch (before the file's cursor is queued), then the
        // trailing 100 ship in a second batch that advances the cursor. Proves the
        // straddle: cursor advances only after the LAST batch carrying the file.
        let resilient = healthy();
        let mut git = NoGit;
        let mut uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let mut events = Vec::with_capacity(1100);
        for i in 0..1100u32 {
            let (h, m) = (i / 60, i % 60);
            events.push(ev("s1", &format!("2026-07-16T{h:02}:{m:02}:00.000Z")));
        }
        let t = run_scan_over_jobs(
            vec![job("/big.jsonl")],
            "dev1",
            "9.9.9",
            "local",
            opts(None, false),
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &mut uploader,
            &mut cursors,
            &mut obs,
        )
        .await;

        assert!(!t.held);
        // Two batches: the 1000-event mid-file flush + the 100-event trailing one.
        assert_eq!(t.batches_uploaded, 2);
        assert_eq!(t.events_uploaded, 1100);
        assert_eq!(uploader.uploaded.len(), 2);
        assert_eq!(uploader.uploaded[0].0, 1000);
        assert_eq!(uploader.uploaded[1].0, 100);
        // Cursor advanced exactly once, after the final batch.
        assert!(cursors.get_cursor("/big.jsonl").is_some());
    }

    #[tokio::test]
    async fn streams_a_large_file_in_many_chunks_with_backpressure() {
        // 1100 events emitted in 11 chunks of 100 — MORE chunks than
        // STREAM_CHANNEL_CAP (4), so the spawn-blocking parser parks on a full
        // channel (backpressure) while batches flush. The batching is identical to
        // a single-chunk emission (1000 + 100), proving the bridge is transparent:
        // memory stays bounded even though the "file" is streamed piecemeal.
        let resilient = healthy();
        let mut git = NoGit;
        let mut uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let mut events = Vec::with_capacity(1100);
        for i in 0..1100u32 {
            let (h, m) = (i / 60, i % 60);
            events.push(ev("s1", &format!("2026-07-16T{h:02}:{m:02}:00.000Z")));
        }
        let t = run_scan_over_jobs(
            vec![job("/big.jsonl")],
            "dev1",
            "9.9.9",
            "local",
            opts(None, false),
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            &mut git,
            None,
            parse_in_chunks(events, 100),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &mut uploader,
            &mut cursors,
            &mut obs,
        )
        .await;

        assert!(!t.held);
        assert_eq!(t.events_uploaded, 1100);
        assert_eq!(uploader.uploaded.len(), 2);
        assert_eq!(uploader.uploaded[0].0, 1000);
        assert_eq!(uploader.uploaded[1].0, 100);
        assert!(cursors.get_cursor("/big.jsonl").is_some());
    }
}
