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

use modelstat_ingest::accounts::Accounts;
use modelstat_ingest::state::FileCursor;
use modelstat_parsers::{GitEnrichment, ParseResult, ToolCallDraft};
use modelstat_pipeline::{Embedder, LinkExtractor, ResilientSummarizer, Summarizer};
use modelstat_redact::NerModel;
use modelstat_wire::{IngestBatch, RawEvent, Segment};

use futures_util::StreamExt;

use crate::discover_jobs::{ParserKind, ScanJob};
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
/// How far behind the watermark a non-positional source (Cursor) re-ships on
/// every scan. A chat row can be written while the assistant is still streaming
/// into it, so freezing it the instant it is first seen would store a
/// half-written message forever. Re-sending a settled row is free — ids are
/// deterministic and the server upserts — so a short overlap buys self-healing.
const RESHIP_LAG_MS: i64 = 10 * 60 * 1000;

/// Most upload futures alive at once. A ceiling, not a policy — the uploader's
/// adaptive gate decides how many actually run, and it can never exceed its own
/// `MAX_CONCURRENCY`. Matching that here keeps this from being the binding
/// constraint while still bounding the futures a flush allocates.
const UPLOAD_FANOUT: usize = modelstat_ingest::upload_gate::MAX_CONCURRENCY;

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
    /// `&self`, not `&mut self`: a flush uploads its batches CONCURRENTLY, and
    /// an exclusive borrow would make that impossible. Implementors keep any
    /// mutable state behind interior mutability (the real one already did —
    /// every `DeviceApi` method takes `&self`).
    ///
    /// Spelled as an explicit `impl Future + Send` rather than `async fn`
    /// because the concurrent flush lives inside a boxed `Send` task: with a
    /// bare `async fn` the compiler cannot promise the returned future is `Send`
    /// for every lifetime, and the scan fails to compile with "implementation of
    /// `Send` is not general enough".
    fn upload(
        &self,
        batch: &IngestBatch,
        raw: bool,
    ) -> impl std::future::Future<Output = Result<u64, Hold>> + Send;
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
    run_events: &mut BTreeMap<String, Vec<RawEvent>>,
    accounts: &Accounts,
    resilient: &ResilientSummarizer<S>,
    embedder: &E,
    ner: &N,
    git: &mut G,
    extract_links: Option<&LinkExtractor<'_>>,
    correct_events: &mut CE,
    uploader: &U,
    cursors: &mut (dyn CursorStore + Send),
    observer: &mut (dyn ScanObserver + Send),
    tallies: &mut ScanTallies,
) -> Result<(), Hold>
where
    S: Summarizer,
    E: Embedder,
    N: NerModel,
    G: GitEnrichment + Send,
    // `Sync` because the concurrent flush shares `&U` across in-flight uploads.
    U: BatchUploader + Sync,
    CE: FnMut(Vec<RawEvent>) -> Vec<RawEvent>,
{
    if buffer.is_empty() && tool_buffer.is_empty() {
        // Nothing to ship — but the files that got here ARE fully processed: they
        // parsed clean and every event they produced was already below the
        // `shipped_below` floor. Their cursors still have to advance, or a file
        // that grew without yielding anything new re-parses on every cycle forever.
        for (path, cursor) in pending_cursors.drain(..) {
            cursors.set_cursor(&path, cursor);
        }
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
        run_events,
        accounts,
    )
    .await;
    let batches = match outcome {
        // Engine down / cloud NER unavailable → hold the whole flush, never a
        // partial batch (that would advance the cursor past un-summarised events).
        FlushOutcome::Held => return Err(Hold),
        FlushOutcome::Ready(b) => b,
    };
    // Ship this flush's batches CONCURRENTLY, because in cloud mode each batch is
    // one session and each POST makes the edge summarise it with an LLM — so a
    // flush covering a dozen sessions used to be a dozen seconds-long round trips,
    // one after another.
    //
    // The number actually in flight is NOT decided here: the uploader holds an
    // adaptive gate fed by the server's own 429s (`modelstat_ingest::upload_gate`),
    // so this only has to stop being the thing that serialises. The bound below
    // is a ceiling on futures alive at once, not a concurrency policy.
    //
    // The memory ceiling this file promises is untouched: `batches` is the whole
    // flush either way — the sequential loop held it too — so what is new is only
    // the handful of request bodies in flight, each a slice of that same flush.
    //
    // The loss-proof contract is unchanged, and depends on it: the cursor
    // advance sits AFTER this whole block, so a hold anywhere in the fan-out
    // advances nothing and the same files re-parse next scan.
    for pb in &batches {
        observer.on_upload(pb.batch.events.len(), pb.segment_count);
    }
    // Built with a plain loop rather than `.map(|pb| async move { … })`: a
    // closure returning a future that BORROWS its argument has to satisfy
    // `FnOnce(&PreparedBatch)` for every lifetime, which it cannot, and the
    // error surfaces far away as "implementation of `Send`/`FnOnce` is not
    // general enough" where the daemon boxes this future. A loop gives each
    // future one concrete borrow and no closure at all.
    let up: &U = uploader;
    let mut futures = Vec::with_capacity(batches.len());
    for pb in &batches {
        futures.push(async move { (pb, up.upload(&pb.batch, pb.raw).await) });
    }
    let mut inflight = futures_util::stream::iter(futures).buffer_unordered(UPLOAD_FANOUT);
    let mut held: Option<Hold> = None;
    while let Some((pb, result)) = inflight.next().await {
        match result {
            Ok(accepted) => {
                tallies.batches_uploaded += 1;
                tallies.events_uploaded += accepted;
                tallies.segments_uploaded += pb.segment_count as u64;
                observer.on_uploaded(accepted as usize, pb.segment_count);
            }
            // First hold wins and we stop: dropping the stream cancels what is
            // still in flight and starts nothing new, which is what the
            // sequential loop did by returning early. Anything already accepted
            // server-side is re-sent next scan and deduped by id, because no
            // cursor moved.
            Err(h) => {
                held = Some(h);
                break;
            }
        }
    }
    drop(inflight);
    if let Some(h) = held {
        return Err(h);
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
    uploader: &U,
    // `+ Send` on the trait objects: the scan runs in the daemon's tokio-spawned
    // single-flight task, whose future must be `Send` — and a `dyn Trait` erases
    // the concrete type's Send-ness unless the bound is spelled out. Every real
    // impl (RuntimeState, StatusObserver, RealGitEnrichment) is `Send`.
    cursors: &mut (dyn CursorStore + Send),
    observer: &mut (dyn ScanObserver + Send),
    // Which provider account is logged in, and since when — the snapshot each
    // shipped session is named against. Injected like every other dependency
    // here; an empty map simply names nothing and the server infers.
    accounts: &Accounts,
) -> ScanTallies
where
    S: Summarizer,
    E: Embedder,
    N: NerModel,
    G: GitEnrichment + Send,
    // `Sync` because the concurrent flush shares `&U` across in-flight uploads.
    U: BatchUploader + Sync,
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
    // The same accumulation for cloud mode, which produces no local segments: its
    // session metadata is recomputed from the run-long (excerpt-shed) turns, so a
    // later flush's partial view can't overwrite a richer earlier one.
    let mut run_events: BTreeMap<String, Vec<RawEvent>> = BTreeMap::new();

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
                &mut run_events,
                accounts,
                resilient,
                embedder,
                ner,
                &mut *git,
                extract_links,
                &mut correct_events,
                uploader,
                &mut *cursors,
                &mut *observer,
                &mut tallies,
            )
            .await
        };
    }

    let total = ordered.len();
    // Consumed, not iterated by reference: holding a `&ScanJob` (and the slice
    // iterator behind it) across the awaits below makes this future's `Send`-ness
    // depend on a specific lifetime, and the daemon boxes it into a `Send` task —
    // which fails to compile as "implementation of `Send` is not general enough"
    // once the flush inside also holds borrowed futures. Owning each job sidesteps
    // the whole question.
    for (i, job) in ordered.into_iter().enumerate() {
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
        // Everything below this byte offset already landed on a CONFIRMED upload,
        // so this scan parses it but does not re-send it. Without the floor a live
        // transcript re-ships in FULL every time the session appends a line — a
        // 67MB/17k-event file re-summarised + re-uploaded per cycle, so the tray
        // never leaves "Uploading <BATCH_MAX_EVENTS> events".
        //
        // The floor gates the SEND, not the READ: the whole file is still parsed
        // because the parsers carry cross-line state (model attribution,
        // tool_use ↔ tool_result pairing) that a mid-file start would lose.
        //
        // Taken only when the file GREW in place (`cs.size >= cur.size`) — a
        // truncated/rewritten file invalidates every recorded offset, so it
        // re-ships whole. The eager force scan re-uploads a processed session on
        // purpose, so it never takes a floor.
        let shipped_below: u64 = match (opts.force_read_all, cur.as_ref(), cs.as_ref()) {
            (false, Some(cur), Some(cs)) if cs.size >= cur.size => cur.size,
            _ => 0,
        };
        // A key/value source (Cursor) has no byte coordinate: its floor is a
        // timestamp watermark carried on the job, and the parser applies it
        // (its rows carry no cross-record state, unlike a transcript line).
        // Never on a force scan — that exists to re-upload on purpose.
        let mut job = job;
        job.since_ms = match (opts.force_read_all, job.kind, cur.as_ref()) {
            (false, ParserKind::Cursor, Some(cur)) => cur
                .shipped_through_ms
                .map(|ms| ms.saturating_sub(RESHIP_LAG_MS)),
            _ => None,
        };
        let job = &job;
        // Highest record instant actually buffered from this file — the next
        // watermark, applied only by a successful flush (below), exactly like a
        // byte cursor.
        let mut max_record_ms: Option<i64> = None;
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
                // Parsed for its cross-line state, but already shipped — drop it
                // before the buffer so it costs no summarise + no upload. An event
                // with no recorded offset can't be placed, so it always ships.
                if e.source_byte_offset.is_some_and(|o| o < shipped_below) {
                    continue;
                }
                if job.kind == ParserKind::Cursor {
                    if let Some(ms) = chrono::DateTime::parse_from_rfc3339(&e.ts)
                        .ok()
                        .map(|d| d.timestamp_millis())
                    {
                        max_record_ms = Some(max_record_ms.map_or(ms, |m: i64| m.max(ms)));
                    }
                }
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
                modelstat_log::log_error!("parse failed for {}: {e}", job.path);
                continue;
            }
            Err(join) => {
                modelstat_log::log_error!("parse task panicked for {}: {join}", job.path);
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
        // Drafts deliberately take NO `shipped_below` floor. A draft's
        // status/latency/result-size are filled in-place when its `tool_result`
        // line is paired, which can happen many lines — and scans — after the
        // `tool_use` that created it. Flooring on the draft's own offset would
        // strand exactly those completions as forever-pending. They re-ship
        // instead: `tc_` ids are deterministic, so the server upserts, and drafts
        // are a small fraction of a transcript next to its events.
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
        if let Some(mut cs) = cs {
            // Keep the previous watermark when this pass shipped nothing new, so
            // a quiet scan never rewinds the floor.
            cs.shipped_through_ms = match (
                max_record_ms,
                cur.as_ref().and_then(|c| c.shipped_through_ms),
            ) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
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
    // Records behind a `Mutex` because uploads overlap now — the real uploader
    // takes `&self` for the same reason.
    #[derive(Default)]
    struct RecordingUploader {
        /// (event_count, raw) per committed batch.
        uploaded: Mutex<Vec<(usize, bool)>>,
        /// `source_event_id`s of EVERY upload attempt (held or committed) — lets a
        /// test prove a crash-held batch and its resend are byte-identical, so the
        /// server dedupes on id (no duplicates after a crash).
        attempts: Mutex<Vec<Vec<String>>>,
        hold: bool,
    }
    impl RecordingUploader {
        fn uploaded(&self) -> Vec<(usize, bool)> {
            self.uploaded.lock().unwrap().clone()
        }
        fn attempts(&self) -> Vec<Vec<String>> {
            self.attempts.lock().unwrap().clone()
        }
    }
    impl BatchUploader for RecordingUploader {
        async fn upload(&self, batch: &IngestBatch, raw: bool) -> Result<u64, Hold> {
            self.attempts.lock().unwrap().push(
                batch
                    .events
                    .iter()
                    .map(|e| e.source_event_id.clone())
                    .collect(),
            );
            if self.hold {
                return Err(Hold);
            }
            let n = batch.events.len();
            self.uploaded.lock().unwrap().push((n, raw));
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
        }
    }

    /// Stamp an event with the byte offset it was parsed from — what the real
    /// streaming parsers record and what the `shipped_below` floor reads.
    fn at(mut e: RawEvent, offset: u64) -> RawEvent {
        e.source_byte_offset = Some(offset);
        e
    }

    /// A cursor for a file whose first `size` bytes are confirmed-shipped. The
    /// tail hash deliberately differs from `checksum`'s, so the unchanged-guard
    /// lets the file through (it GREW) and the floor is what does the work.
    fn confirmed_through(size: u64) -> FileCursor {
        FileCursor {
            shipped_through_ms: None,
            size,
            mtime: 1,
            tail_hash: "older".into(),
        }
    }

    fn job(path: &str) -> ScanJob {
        ScanJob {
            agent_label: None,
            since_ms: None,
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
            shipped_through_ms: None,
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
        let uploader = RecordingUploader::default();
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert_eq!(t.files_scanned, 2);
        assert_eq!(t.files_unchanged, 0);
        assert!(!t.held);
        assert!(t.batches_uploaded >= 1);
        // Local mode never ships raw.
        assert!(uploader.uploaded().iter().all(|(_, raw)| !raw));
        // Both files' cursors advanced only after their events committed.
        assert!(cursors.get_cursor("/a.jsonl").is_some());
        assert!(cursors.get_cursor("/b.jsonl").is_some());
    }

    #[tokio::test]
    async fn skips_a_file_whose_size_and_tail_are_unchanged() {
        let resilient = healthy();
        let mut git = NoGit;
        let uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        // Pre-seed the cursor to EXACTLY what checksum() will report for /a.jsonl.
        cursors.set_cursor(
            "/a.jsonl",
            FileCursor {
                shipped_through_ms: None,
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert_eq!(t.files_unchanged, 1);
        assert_eq!(t.files_scanned, 0);
        assert_eq!(t.batches_uploaded, 0);
        assert!(uploader.uploaded().is_empty());
    }

    #[tokio::test]
    async fn force_read_all_bypasses_the_unchanged_skip() {
        let resilient = healthy();
        let mut git = NoGit;
        let uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        cursors.set_cursor(
            "/a.jsonl",
            FileCursor {
                shipped_through_ms: None,
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
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
        let uploader = RecordingUploader::default();
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(t.held);
        assert_eq!(t.batches_uploaded, 0);
        // Never-drop: the file's cursor stays put so it re-parses next cycle.
        assert!(cursors.get_cursor("/a.jsonl").is_none());
        assert!(uploader.uploaded().is_empty());
    }

    #[tokio::test]
    async fn upload_hold_leaves_cursors_un_advanced() {
        let resilient = healthy();
        let mut git = NoGit;
        // Engine is healthy (batches build), but every upload holds.
        let uploader = RecordingUploader {
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(t.held);
        assert_eq!(t.batches_uploaded, 0);
        assert!(cursors.get_cursor("/a.jsonl").is_none());
    }

    #[tokio::test]
    async fn a_held_flush_is_re_shipped_identically_and_commits_on_retry() {
        // The crash story end-to-end: an upload can't commit (offline / server 5xx /
        // a crash between commit and cursor-persist), so the cursor never advances
        // and the batch re-ships next cycle. Proves the whole never-drop cycle:
        //   1. hold → no loss (cursor un-advanced);
        //   2. the retry ships the IDENTICAL events (same source_event_ids → the
        //      server dedupes on id: no duplicates after a crash);
        //   3. the commit finally advances the cursor.
        let resilient = healthy();
        let mut git = NoGit;
        let mut uploader = RecordingUploader {
            hold: true,
            ..Default::default()
        };
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let events = vec![
            ev("s1", "2026-07-16T10:00:00.000Z"),
            ev("s1", "2026-07-16T10:01:00.000Z"),
        ];

        // Cycle 1 — the upload holds.
        let t1 = run_scan_over_jobs(
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
            parse_with(events.clone()),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;
        assert!(t1.held, "cycle 1 must hold");
        assert!(
            cursors.get_cursor("/a.jsonl").is_none(),
            "a held flush must NOT advance the cursor (no loss)"
        );

        // Cycle 2 — the server recovers; the same file re-ships and commits.
        uploader.hold = false;
        let t2 = run_scan_over_jobs(
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
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;
        assert!(!t2.held, "cycle 2 must commit");
        assert!(
            cursors.get_cursor("/a.jsonl").is_some(),
            "a committed flush advances the cursor"
        );

        // The held attempt and the successful resend carried the IDENTICAL events —
        // same source_event_ids, so the server dedupes: a crash never duplicates.
        assert_eq!(
            uploader.attempts().len(),
            2,
            "one held attempt + one resend"
        );
        assert!(!uploader.attempts()[0].is_empty());
        assert_eq!(
            uploader.attempts()[0],
            uploader.attempts()[1],
            "resend must carry byte-identical event ids (server dedupes on id)"
        );
    }

    #[tokio::test]
    async fn file_cap_stops_early_and_sets_more_pending() {
        let resilient = healthy();
        let mut git = NoGit;
        let uploader = RecordingUploader::default();
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
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
        let uploader = RecordingUploader::default();
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(!t.held);
        // Two batches: the 1000-event mid-file flush + the 100-event trailing one.
        assert_eq!(t.batches_uploaded, 2);
        assert_eq!(t.events_uploaded, 1100);
        assert_eq!(uploader.uploaded().len(), 2);
        assert_eq!(uploader.uploaded()[0].0, 1000);
        assert_eq!(uploader.uploaded()[1].0, 100);
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
        let uploader = RecordingUploader::default();
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(!t.held);
        assert_eq!(t.events_uploaded, 1100);
        assert_eq!(uploader.uploaded().len(), 2);
        assert_eq!(uploader.uploaded()[0].0, 1000);
        assert_eq!(uploader.uploaded()[1].0, 100);
        assert!(cursors.get_cursor("/big.jsonl").is_some());
    }

    #[tokio::test]
    async fn a_grown_file_ships_only_the_events_past_its_cursor() {
        // The re-upload treadmill this floor ends: a LIVE transcript grows by a
        // line, so the unchanged-guard rightly lets it through — and the scan then
        // re-parsed and re-shipped the whole file, every cycle, forever. A 67MB /
        // 17k-event session did that on every keystroke, which is why the tray sat
        // on "Uploading <BATCH_MAX_EVENTS> events" and never finished.
        let resilient = healthy();
        let mut git = NoGit;
        let uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        // 60 bytes confirmed-shipped; `checksum` reports the file is now 100.
        cursors.set_cursor("/big.jsonl", confirmed_through(60));
        let events = vec![
            at(ev("s1", "2026-07-16T10:00:00.000Z"), 0), // landed last cycle
            at(ev("s1", "2026-07-16T10:01:00.000Z"), 59), // landed last cycle
            at(ev("s1", "2026-07-16T10:02:00.000Z"), 60), // the new tail
            at(ev("s1", "2026-07-16T10:03:00.000Z"), 80),
        ];
        let t = run_scan_over_jobs(
            vec![job("/big.jsonl")],
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(!t.held);
        // The file was still fully PARSED (cross-line state intact) — only the two
        // already-confirmed events were dropped before the buffer.
        assert_eq!(t.files_scanned, 1);
        assert_eq!(t.events_uploaded, 2);
        assert_eq!(uploader.uploaded().len(), 1);
        assert_eq!(uploader.uploaded()[0].0, 2);
        // Exactly the tail, by id — never the re-shipped head.
        assert_eq!(
            uploader.attempts()[0],
            vec![
                "s1:2026-07-16T10:02:00.000Z".to_string(),
                "s1:2026-07-16T10:03:00.000Z".to_string()
            ]
        );
        // Cursor advanced to the new size, so the next cycle floors at 100.
        assert_eq!(cursors.get_cursor("/big.jsonl").unwrap().size, 100);
    }

    #[tokio::test]
    async fn a_grown_file_with_nothing_new_still_advances_its_cursor() {
        // Every event fell below the floor and the file has no tool drafts, so the
        // flush has nothing to send. The cursor must advance anyway — otherwise the
        // size no longer matches, the unchanged-guard lets the file through again,
        // and it re-parses on every cycle for the rest of the daemon's life.
        let resilient = healthy();
        let mut git = NoGit;
        let uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        cursors.set_cursor("/quiet.jsonl", confirmed_through(60));
        let events = vec![
            at(ev("s1", "2026-07-16T10:00:00.000Z"), 0),
            at(ev("s1", "2026-07-16T10:01:00.000Z"), 40),
        ];
        let t = run_scan_over_jobs(
            vec![job("/quiet.jsonl")],
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(!t.held);
        assert_eq!(t.events_uploaded, 0);
        assert!(uploader.uploaded().is_empty()); // nothing on the wire
        assert_eq!(cursors.get_cursor("/quiet.jsonl").unwrap().size, 100);
    }

    #[tokio::test]
    async fn a_shrunken_file_re_ships_whole_because_its_offsets_are_meaningless() {
        // Truncated / rewritten in place: recorded offsets no longer point at the
        // same lines, so flooring on them would silently drop real events. Only
        // append-in-place growth (`cs.size >= cur.size`) earns a floor.
        let resilient = healthy();
        let mut git = NoGit;
        let uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        // Confirmed through 500 bytes, but `checksum` now reports only 100.
        cursors.set_cursor("/rewritten.jsonl", confirmed_through(500));
        let events = vec![
            at(ev("s1", "2026-07-16T10:00:00.000Z"), 0),
            at(ev("s1", "2026-07-16T10:01:00.000Z"), 40),
        ];
        let t = run_scan_over_jobs(
            vec![job("/rewritten.jsonl")],
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert_eq!(t.events_uploaded, 2); // both, despite offsets below 500
    }

    #[tokio::test]
    async fn the_eager_force_scan_takes_no_floor() {
        // `force_read_all` exists so a session the daemon already processed
        // re-uploads on demand. A floor would make it ship nothing at all.
        let resilient = healthy();
        let mut git = NoGit;
        let uploader = RecordingUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        cursors.set_cursor("/done.jsonl", confirmed_through(100));
        let events = vec![
            at(ev("s1", "2026-07-16T10:00:00.000Z"), 0),
            at(ev("s1", "2026-07-16T10:01:00.000Z"), 40),
        ];
        let t = run_scan_over_jobs(
            vec![job("/done.jsonl")],
            "dev1",
            "9.9.9",
            "local",
            opts(None, true), // force_read_all
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
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert_eq!(t.events_uploaded, 2);
    }

    /// A LIVE NER model, which a cloud test needs: cloud egress fail-closes on a
    /// redactor that is missing OR ineffective, and `ner_active` decides which by
    /// probing with a sentinel name and checking it really disappeared. So this
    /// answers the probe and finds nothing in the test turns themselves.
    struct LiveNer;
    impl NerModel for LiveNer {
        fn classify(&self, text: &str) -> Option<Vec<modelstat_redact::NerToken>> {
            let mut out = Vec::new();
            if let Some(i) = text.find("Katherine Johnson") {
                let tok =
                    |entity: &str, word: &str, a: usize, b: usize| modelstat_redact::NerToken {
                        entity: entity.into(),
                        word: word.into(),
                        start: Some(a),
                        end: Some(b),
                    };
                out.push(tok("B-PER", "Katherine", i, i + 9));
                out.push(tok("I-PER", "Johnson", i + 10, i + 17));
            }
            Some(out)
        }
    }

    /// Watches the fan-out: counts how many uploads are alive at the same
    /// moment, and can hold one named session's batch.
    #[derive(Default)]
    struct ConcurrentUploader {
        inflight: AtomicUsize,
        peak: AtomicUsize,
        /// The session whose batch holds — chosen by NAME, not by call order, so
        /// the test does not depend on how the runtime interleaves.
        hold_session: Option<&'static str>,
    }
    impl BatchUploader for ConcurrentUploader {
        async fn upload(&self, batch: &IngestBatch, _raw: bool) -> Result<u64, Hold> {
            let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            // Suspend, the way a real POST does while the server summarises. A
            // sibling upload is polled meanwhile — if the loop were still
            // sequential, `peak` could never exceed one.
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            let session = batch.events.first().map(|e| e.session_id.as_str());
            if self.hold_session.is_some() && self.hold_session == session {
                return Err(Hold);
            }
            Ok(batch.events.len() as u64)
        }
    }

    /// Three sessions in one file, cloud mode ⇒ three batches from ONE flush
    /// (cloud ships one session per raw batch). They must be in flight together;
    /// uploading them one at a time is what made a backlog take hours, since each
    /// POST waits on a server-side summarise.
    #[tokio::test]
    async fn uploads_in_one_flush_overlap_instead_of_queueing() {
        let resilient = healthy();
        let mut git = NoGit;
        let uploader = ConcurrentUploader::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let events = vec![
            ev("s1", "2026-07-16T10:00:00.000Z"),
            ev("s2", "2026-07-16T10:01:00.000Z"),
            ev("s3", "2026-07-16T10:02:00.000Z"),
        ];
        let t = run_scan_over_jobs(
            vec![job("/three.jsonl")],
            "dev1",
            "9.9.9",
            "cloud",
            opts(None, false),
            &resilient,
            &NoEmbedder,
            &LiveNer,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(!t.held);
        assert_eq!(t.batches_uploaded, 3, "one batch per session");
        assert_eq!(t.events_uploaded, 3);
        assert!(
            uploader.peak.load(Ordering::SeqCst) > 1,
            "uploads must overlap, saw peak in-flight of {}",
            uploader.peak.load(Ordering::SeqCst)
        );
        // Concurrency changes nothing about the cursor contract: all three
        // committed, so the file advances exactly as before.
        assert!(cursors.get_cursor("/three.jsonl").is_some());
    }

    /// The loss-proof half of the fan-out: ONE batch holding leaves the file's
    /// cursor untouched, even though its siblings committed. The whole file
    /// re-parses next cycle and the server dedupes the resends by event id —
    /// which is exactly what the sequential loop did by returning early.
    #[tokio::test]
    async fn a_hold_anywhere_in_the_fanout_advances_no_cursor() {
        let resilient = healthy();
        let mut git = NoGit;
        let uploader = ConcurrentUploader {
            hold_session: Some("s2"),
            ..Default::default()
        };
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let events = vec![
            ev("s1", "2026-07-16T10:00:00.000Z"),
            ev("s2", "2026-07-16T10:01:00.000Z"),
            ev("s3", "2026-07-16T10:02:00.000Z"),
        ];
        let t = run_scan_over_jobs(
            vec![job("/three.jsonl")],
            "dev1",
            "9.9.9",
            "cloud",
            opts(None, false),
            &resilient,
            &NoEmbedder,
            &LiveNer,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &uploader,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(t.held, "one held batch holds the flush");
        assert!(
            cursors.get_cursor("/three.jsonl").is_none(),
            "never-drop: a held batch anywhere in the fan-out advances NO cursor"
        );
        // The drain stops at the hold: nothing after it is counted and nothing new
        // is started. A sibling ALREADY in flight may still finish — harmless
        // precisely because its receipt moves no cursor either.
        assert!(
            t.batches_uploaded < 3,
            "the tally stops at the first hold, counted {}",
            t.batches_uploaded
        );
        assert_eq!(
            t.events_uploaded, t.batches_uploaded,
            "one event per session, so counted events track counted batches"
        );
    }
}
