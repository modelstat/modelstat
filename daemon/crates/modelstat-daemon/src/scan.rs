//! The scan loop — the shared engine behind both `scanAll` and `scanSession`.
//! Given an ordered job list it parses each transcript, buffers the events,
//! composes batches via [`build_flush_batches`], parks them in the durable
//! [`crate::spool`], and advances each file's cursor **only once its batch is on
//! the platter** — so a mid-scan failure re-parses the same files next cycle.
//! Good data is never dropped.
//!
//! The cursor rule used to be "only after a confirmed upload", which coupled the
//! expensive half of the daemon (redaction) to the unreliable half (the network):
//! every held upload threw away finished work and made the next cycle redo it.
//! Durability now means "fsynced locally", and [`crate::uploader`] carries it the
//! rest of the way, retrying forever without re-running a single model pass.
//!
//! Everything the loop does I/O with is an injected seam (`parse`, `checksum`,
//! `correct_events`, `enrich_scripts`, the [`BatchSink`], the [`CursorStore`]) so
//! the whole state machine is unit-testable against a fake engine + fake sink
//! without touching a real `.jsonl` or the network. The daemon-main wires the real
//! adapters (the [`crate::flush`] doc names them).

use std::collections::BTreeMap;

use modelstat_ingest::accounts::Accounts;
use modelstat_ingest::state::FileCursor;
use modelstat_parsers::{
    merge_session_actors, GitEnrichment, ParseResult, SessionActors, ToolCallDraft,
};
use modelstat_pipeline::{Embedder, LinkExtractor, ResilientSummarizer, Summarizer};
use modelstat_redact::PiiModel;
use modelstat_wire::{RawEvent, Segment};

use crate::discover_jobs::{ParserKind, ScanJob};
use crate::flush::{build_flush_batches, FlushOutcome, PreparedBatch};

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

const STREAM_CHANNEL_CAP: usize = 4;

/// How many files may be in flight at once: the one whose events are being
/// flushed plus the next few PARSING ahead on their blocking threads. While a
/// flush waits on the network (cloud redaction round-trips), the upcoming
/// files' parsers fill their bounded channels and park — so the wire wait and
/// the parse work overlap instead of taking turns. Memory stays flat: each
/// file ahead holds at most [`STREAM_CHANNEL_CAP`] chunks, and files are
/// CONSUMED strictly in order, so cursor semantics don't change.
const PIPELINE_FILES: usize = 3;

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
    /// Batches redacted and parked durably in the spool. NOT "uploaded" — the
    /// uploader owns that word and reports it separately, because telling someone
    /// their data is sent when it is sitting on their own disk is a lie the tray
    /// used to be able to tell during an outage.
    pub batches_spooled: u64,
    pub events_spooled: u64,
    pub segments_spooled: u64,
    /// The per-cycle file cap was hit with files still pending — the caller
    /// should re-scan promptly to drain the rest (newest-first).
    pub more_pending: bool,
    /// A flush was HELD (engine down, or the spool would not take it): the
    /// buffered files' cursors were left un-advanced and will be retried next
    /// cycle.
    pub held: bool,
    /// Files read WHOLE that parsed to nothing at all — no events, no tool
    /// calls — while plainly containing bytes. Exactly the shape of an
    /// upstream schema move (Cursor's `ai_code_hashes` sat dead for weeks this
    /// way): the parser matched the artefact, understood none of it, and the
    /// silence was indistinguishable from an empty file. Loud now.
    pub files_silent: usize,
    /// Files whose parse ERRORED. The cursor is deliberately left where it was
    /// — never advance past bytes nobody read — which means the file retries
    /// next cycle at full parse cost. Counted so that retry is visible rather
    /// than a silent treadmill: a number that climbs every cycle is a file the
    /// daemon cannot read at all, not a transient.
    pub files_failed: usize,
    /// Records no parser arm could read, by the kind string the source states
    /// about itself — the per-scan fold of every parse's
    /// [`modelstat_parsers::SkipLedger`].
    pub skipped_kinds: BTreeMap<String, u64>,
}

/// One log line per distinct (file, error) per process.
///
/// The daemon re-scans on a timer, so an unreadable file would otherwise print
/// the same line every cycle for as long as it sits on disk. The pair is the key
/// rather than the path alone: a file that starts failing DIFFERENTLY has
/// changed, and that is worth saying again.
fn warn_parse_failure(path: &str, error: &str) {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeSet::new()));
    let mut guard = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.insert(format!("{path}\u{0}{error}")) {
        modelstat_log::log_warn!(
            "parse failed for {path}: {error} — its cursor stays put (nothing advances past \
             unread bytes), so this file retries every cycle until it is readable"
        );
    }
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

/// The hold signal: a flush could not be made durable (the summariser engine is
/// down, or the spool would not take the batch). Cursors are NOT advanced, so the
/// same events are re-read next cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold;

/// Where a finished batch goes. The daemon backs this with the on-disk
/// [`crate::spool::Spool`]; tests use a fake that records or refuses.
///
/// This used to be `BatchUploader` — the scan POSTed each batch itself and only
/// advanced a cursor once the server confirmed it. That coupling is what made an
/// outage expensive: a held upload discarded the redacted batch, and the next
/// cycle re-ran the PII model over the very same turns to rebuild it. Now the
/// scan's job ends at "durably on this disk", which it can always achieve, and
/// the uploader retries from there on its own clock.
///
/// Synchronous on purpose: parking a batch is one `write` + `fsync` + `rename`,
/// and making it async bought nothing but the `Send`-bound gymnastics the
/// concurrent shipper needed.
pub trait BatchSink {
    /// `Ok(())` = the batch is durable and the caller may advance its cursors.
    /// `Err(Hold)` = it is not; stop the scan and retry the same events later.
    fn accept(&self, batch: &PreparedBatch) -> Result<(), Hold>;
}

/// Progress sink for a scan — the daemon updates `last-status.json` + the tray
/// from these; tests pass `&mut ()`. Port of TS `ScanCallbacks` (minus the
/// per-segment `onProgress`, which the pure batch builder doesn't surface).
pub trait ScanObserver {
    /// Fresh activity on a session this scan just read: newest event instant +
    /// the agent + a human label (the cwd's last component). Fired per file per
    /// session, deduped within the file — cheap, and it is the ONLY signal that
    /// lets a UI answer "what is running right now".
    fn on_session_activity(
        &mut self,
        session_id: &str,
        agent: &str,
        label: Option<&str>,
        last_ms: i64,
    ) {
        let _ = (session_id, agent, label, last_ms);
    }

    /// Before parsing each file — `index` 0-based, `total` = files discovered.
    fn on_file(&mut self, path: &str, index: usize, total: usize) {
        let _ = (path, index, total);
    }
    /// One batch is redacted and durably parked. It is NOT sent yet — the
    /// uploader reports that, and a reader must be able to tell the two apart
    /// (during an outage this keeps climbing while nothing leaves the machine).
    fn on_spooled(&mut self, events: usize, segments: usize) {
        let _ = (events, segments);
    }
}
impl ScanObserver for () {}

/// One flush's finished output: the batches to park, and the cursors that may
/// advance once they are parked.
struct PreparedFlush {
    batches: Vec<PreparedBatch>,
    /// Files fully parsed as of this flush.
    cursors: Vec<(String, FileCursor)>,
}

/// Assemble whatever is buffered into shippable batches — the expensive half of
/// the daemon (summarise + the on-device PII pass). Empties `buffer`/`tool_buffer`
/// and takes the pending cursors; on `Err(Hold)` the caller stops the scan and the
/// un-advanced files retry next cycle.
#[allow(clippy::too_many_arguments)]
async fn prepare_flush<S, E, N, G, CE>(
    device_id: &str,
    daemon_version: &str,
    mode: &str,
    buffer: &mut Vec<RawEvent>,
    tool_buffer: &mut Vec<ToolCallDraft>,
    pending_cursors: &mut Vec<(String, FileCursor)>,
    run_segments: &mut BTreeMap<String, Vec<Segment>>,
    run_events: &mut BTreeMap<String, Vec<RawEvent>>,
    run_actors: &SessionActors,
    generation: &crate::flush::ScanGeneration,
    accounts: &Accounts,
    resilient: &ResilientSummarizer<S>,
    embedder: &E,
    redactor: &N,
    git: &mut G,
    extract_links: Option<&LinkExtractor<'_>>,
    correct_events: &mut CE,
) -> Result<PreparedFlush, Hold>
where
    S: Summarizer,
    E: Embedder,
    N: PiiModel,
    G: GitEnrichment + Send,
    CE: FnMut(Vec<RawEvent>) -> Vec<RawEvent>,
{
    // The cursors queued so far travel WITH this flush, even when there is
    // nothing to ship: a file that parsed clean but produced only events already
    // below its `shipped_below` floor still has to advance, or it re-parses every
    // cycle forever.
    let cursors = std::mem::take(pending_cursors);
    if buffer.is_empty() && tool_buffer.is_empty() {
        return Ok(PreparedFlush {
            batches: Vec::new(),
            cursors,
        });
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
        redactor,
        // Fresh reborrow per flush — `git` outlives every flush, so this dodges
        // the reborrow-across-await lifetime trap (see flush.rs LESSON). `+ Send`
        // so the erased trait object stays `Send` for the spawned scan task.
        Some(&mut *git as &mut (dyn GitEnrichment + Send)),
        extract_links,
        run_segments,
        run_events,
        run_actors,
        generation,
        accounts,
    )
    .await;
    match outcome {
        // Engine down / the redactor unable to answer → hold the whole flush, never a
        // partial batch (that would advance the cursor past un-summarised events).
        FlushOutcome::Held => Err(Hold),
        FlushOutcome::Ready(batches) => Ok(PreparedFlush { batches, cursors }),
    }
}

/// The shared parse → summarise → redact → park loop over an ordered job list.
/// Holds at most ~one batch (plus [`PIPELINE_FILES`] files' bounded parse
/// channels) in memory and advances each file's cursor only once that batch is
/// durable, so a mid-scan failure re-tries the same events next run. Files are
/// consumed strictly in job order; only their PARSING runs ahead.
/// Both the incremental `scan_all` and the eager single-session scan funnel
/// through here; their only differences are [`RunScanOptions`].
///
/// Uploading is deliberately NOT here — [`crate::uploader`] drains the spool on
/// its own clock. That separation is what keeps a network outage from costing CPU:
/// this loop only ever does the expensive work once per turn.
///
/// The many parameters are the seams that let this be tested without real I/O:
/// `parse`/`checksum` read a job (daemon wires `parse_job`/`quick_checksum`),
/// `correct_events` applies authoritative-git correction (daemon wires
/// `resolve_authoritative_git` over its own resolver — kept separate from `git`
/// so two callers don't double-borrow one resolver), `exists`/`read_file` probe +
/// read referenced script files for the best-effort script-summary enrichment
/// (daemon wires the real `Path::exists` + a capped file read), `sink`/
/// `cursors`/`observer` are the sinks. `git` is the metadata enrichment the loop
/// OWNS and lends to each flush.
#[allow(clippy::too_many_arguments)]
pub async fn run_scan_over_jobs<S, E, N, G, K, P, C, CE>(
    ordered: Vec<ScanJob>,
    device_id: &str,
    daemon_version: &str,
    mode: &str,
    opts: RunScanOptions,
    resilient: &ResilientSummarizer<S>,
    embedder: &E,
    redactor: &N,
    git: &mut G,
    extract_links: Option<&LinkExtractor<'_>>,
    parse: P,
    checksum: C,
    mut correct_events: CE,
    // `+ Sync` so `&exists`/`&read_file` are `Send` — the scan runs inside the
    // daemon's tokio-spawned single-flight task, whose future must be `Send`.
    exists: &(dyn Fn(&str) -> bool + Sync),
    read_file: &(dyn Fn(&str) -> Option<String> + Sync),
    sink: &K,
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
    N: PiiModel,
    G: GitEnrichment + Send,
    // `Sync` so `&K` stays `Send` — the scan runs inside the daemon's
    // tokio-spawned single-flight task, whose future must be `Send`.
    K: BatchSink + Sync,
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
    // advanced only once the batch carrying their events is durable (never-drop).
    let mut pending_cursors: Vec<(String, FileCursor)> = Vec::new();
    // Segments accumulated per session across the WHOLE run: a big file can split
    // a session across flush boundaries, and its title / call-attribution must
    // read every segment seen so far (else a later partial-view title wins).
    let mut run_segments: BTreeMap<String, Vec<Segment>> = BTreeMap::new();
    // The same accumulation for cloud mode, which produces no local segments: its
    // session metadata is recomputed from the run-long (excerpt-shed) turns, so a
    // later flush's partial view can't overwrite a richer earlier one.
    let mut run_events: BTreeMap<String, Vec<RawEvent>> = BTreeMap::new();
    // This scan's claim (core#701). The id is stable for the whole run and rises
    // between runs, so the server can tell one scan's segments from the last's;
    // `read_whole` collects the sessions this run parsed from byte 0, which are
    // the only ones it may honestly say it restated.
    let mut generation = crate::flush::ScanGeneration {
        id: format!("scan_{}", chrono::Utc::now().timestamp_millis().max(0)),
        read_whole: Default::default(),
    };
    // Every agent-instance each session stated, folded across the whole run: one
    // session's sub-agents are one transcript EACH, so the roster is only
    // complete once every file that mentions the session has been read.
    let mut run_actors: SessionActors = SessionActors::new();

    // Prepare the buffered events into a flush and park it durably. Evaluates to
    // `Result<(), Hold>`; an `Err` stops the scan with cursors un-advanced.
    //
    // This is sequential, and that is a simplification the spool bought. It used
    // to be a small concurrent pipeline — several flushes on the wire at once,
    // outcomes landing out of order, and a "prefix rule" so a late success could
    // never advance a cursor past an earlier flush's un-landed events. All of that
    // existed to keep the wire busy while the parser worked. The uploader is now a
    // separate loop that is ALWAYS free to run, so the wire stays busy on its own,
    // and this side gets to be a straight line: prepare, park, advance.
    macro_rules! flush {
        () => {{
            match prepare_flush(
                device_id,
                daemon_version,
                mode,
                &mut buffer,
                &mut tool_buffer,
                &mut pending_cursors,
                &mut run_segments,
                &mut run_events,
                &run_actors,
                &generation,
                accounts,
                resilient,
                embedder,
                redactor,
                &mut *git,
                extract_links,
                &mut correct_events,
            )
            .await
            {
                Err(Hold) => Err(Hold),
                Ok(prepared) => {
                    let mut parked = Ok(());
                    for pb in &prepared.batches {
                        if sink.accept(pb).is_err() {
                            parked = Err(Hold);
                            break;
                        }
                        tallies.batches_spooled += 1;
                        tallies.events_spooled += pb.batch.events.len() as u64;
                        tallies.segments_spooled += pb.segment_count as u64;
                        observer.on_spooled(pb.batch.events.len(), pb.segment_count);
                    }
                    // All-or-nothing: a partial park leaves the tail of this flush
                    // unbuilt, so the cursors must not move past it. The batches
                    // that DID land are re-prepared next cycle and deduped by id
                    // server-side — wasteful, but this only happens when the disk
                    // itself is refusing writes.
                    if parked.is_ok() {
                        for (path, cursor) in prepared.cursors {
                            cursors.set_cursor(&path, cursor);
                        }
                    }
                    parked
                }
            }
        }};
    }

    /// One file whose parse has STARTED: the sync streaming parser is running
    /// on its blocking thread, feeding the bounded channel, parked whenever
    /// the channel is full. Everything decided before the parse (cursor,
    /// checksum, ship floor) travels with it to consumption time.
    struct StartedParse {
        job: ScanJob,
        cur: Option<FileCursor>,
        cs: Option<FileCursor>,
        shipped_below: u64,
        rx: tokio::sync::mpsc::Receiver<Vec<RawEvent>>,
        handle: tokio::task::JoinHandle<std::io::Result<ParseResult>>,
    }

    let total = ordered.len();
    // Consumed, not iterated by reference: holding a `&ScanJob` (and the slice
    // iterator behind it) across the awaits below makes this future's `Send`-ness
    // depend on a specific lifetime, and the daemon boxes it into a `Send` task —
    // which fails to compile as "implementation of `Send` is not general enough"
    // once the flush inside also holds borrowed futures. Owning each job sidesteps
    // the whole question.
    let mut jobs = ordered.into_iter().enumerate();
    // The parse pipeline: files started, oldest first. The front is the file
    // being consumed; the ones behind it are parsing ahead so a flush's network
    // wait and the next files' parse work overlap. Consumption order IS job
    // order, so everything downstream (buffering, flushing, cursors) sees the
    // exact sequence the sequential loop produced.
    let mut in_flight: std::collections::VecDeque<StartedParse> = Default::default();
    // Changed files STARTED (≠ consumed) — the fill gate for `max_files`, so
    // the pipeline never parses a file the cap would have kept this cycle from
    // reaching anyway.
    let mut started_changed = 0usize;

    loop {
        // Fill the pipeline: examine jobs in order (skipping unchanged files)
        // and start parses until enough files are in flight or the per-cycle
        // cap says no more.
        while in_flight.len() < PIPELINE_FILES
            && opts.max_files.is_none_or(|max| started_changed < max)
        {
            let Some((i, job)) = jobs.next() else { break };
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
            started_changed += 1;
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
            // Stream this file's events through a bounded channel while the SYNC
            // streaming parser runs on a spawn-blocking thread. The parser can't await
            // the async flush, so the channel bridges them: the async side drains a
            // chunk, buffers it, and flushes the moment a full batch accumulates; the
            // parser parks on a full channel (backpressure) so it never reads ahead of
            // the flush — memory stays near one batch + the pipeline's channels even
            // mid-file, so a multi-hundred-MB transcript never fully materialises. A
            // held flush stops the scan — no in-flight file's cursor was queued yet
            // (that happens post-parse below), so they all re-parse next cycle.
            let (tx, rx) = tokio::sync::mpsc::channel::<Vec<RawEvent>>(STREAM_CHANNEL_CAP);
            let parse_one = parse.clone();
            let job_owned = job.clone();
            let handle = tokio::task::spawn_blocking(move || {
                let mut emit = move |chunk: Vec<RawEvent>| {
                    // A closed channel (the async side held + returned) just drops the
                    // remainder — the cursor never advanced, so the file re-parses.
                    let _ = tx.blocking_send(chunk);
                };
                parse_one(&job_owned, &mut emit)
            });
            in_flight.push_back(StartedParse {
                job,
                cur,
                cs,
                shipped_below,
                rx,
                handle,
            });
        }
        // Consume the oldest in-flight file; done when the pipeline drained.
        let Some(StartedParse {
            job,
            cur,
            cs,
            shipped_below,
            mut rx,
            handle: parse_handle,
        }) = in_flight.pop_front()
        else {
            break;
        };
        let job = &job;
        tallies.files_scanned += 1;
        // Highest record instant actually buffered from this file — the next
        // watermark, applied only by a successful flush (below), exactly like a
        // byte cursor.
        let mut max_record_ms: Option<i64> = None;
        let mut held = false;
        let mut parsed_events = 0usize;
        // Per-session newest instant seen in this file — feeds the live-session
        // ledger after the loop (deduped here so the observer stays cheap).
        let mut live_seen: BTreeMap<String, (String, Option<String>, i64)> = BTreeMap::new();
        while let Some(chunk) = rx.recv().await {
            parsed_events += chunk.len();
            for e in &chunk {
                if let Some(ms) = chrono::DateTime::parse_from_rfc3339(&e.ts)
                    .ok()
                    .map(|d| d.timestamp_millis())
                {
                    let label = e
                        .cwd
                        .as_deref()
                        .and_then(|c| c.replace('\\', "/").rsplit('/').next().map(str::to_string))
                        .filter(|s| !s.is_empty());
                    let slot = live_seen
                        .entry(e.session_id.clone())
                        .or_insert_with(|| (e.agent.clone(), label.clone(), ms));
                    if ms >= slot.2 {
                        slot.0 = e.agent.clone();
                        if label.is_some() {
                            slot.1 = label;
                        }
                        slot.2 = ms;
                    }
                }
            }
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
                // The cursor is NOT queued below, and that is correct: nothing
                // may advance past bytes nobody read. The cost is that the file
                // re-parses every cycle until it is readable, so the retry is
                // counted and named ONCE — a log line per cycle for a file that
                // will never parse trains its reader to stop looking.
                tallies.files_failed += 1;
                warn_parse_failure(&job.path, &e.to_string());
                continue;
            }
            Err(join) => {
                tallies.files_failed += 1;
                warn_parse_failure(&job.path, &join.to_string());
                continue;
            }
        };
        merge_session_actors(&mut run_actors, std::mem::take(&mut r.session_actors));
        for (kind, n) in std::mem::take(&mut r.skipped_kinds) {
            *tallies.skipped_kinds.entry(kind).or_insert(0) += n;
        }
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
            Some(redactor),
        )
        .await;
        for (sid, (agent, label, ms)) in &live_seen {
            observer.on_session_activity(sid, agent, label.as_deref(), *ms);
        }
        // A WHOLE read (no byte floor, no watermark) of a non-empty file that
        // produced NOTHING is a dialect we no longer understand, not an empty
        // file — the difference between "nothing to say" and "could not read"
        // is the one the Cursor dead-schema weeks were lost to. Incremental
        // reads legitimately parse to nothing new, so they don't count.
        let read_whole = shipped_below == 0 && job.since_ms.is_none();
        // Which sessions this scan may claim to have restated (core#701). Keyed
        // on the sessions the FILE produced, because one transcript is one
        // session's record — a session read whole in this file is one the
        // server may supersede.
        if read_whole {
            for sid in live_seen.keys() {
                generation.read_whole.insert(sid.clone());
            }
        }
        let file_has_bytes = cs.as_ref().is_some_and(|c| c.size > 0);
        if read_whole && file_has_bytes && parsed_events == 0 && r.tool_calls.is_empty() {
            tallies.files_silent += 1;
            modelstat_log::log_warn!(
                "{} parsed to NOTHING ({:?}, {} bytes) — if this repeats, the \
                 app's transcript schema has likely moved and this daemon needs \
                 a parser update",
                job.path,
                job.kind,
                cs.as_ref().map(|c| c.size).unwrap_or(0)
            );
        }
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
    // Park the trailing partial batch. A hold here just leaves it for next cycle.
    if flush!().is_err() {
        tallies.held = true;
    }
    tallies
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover_jobs::ParserKind;
    use crate::testing::AnsweringRedactor;
    use modelstat_ingest::RuntimeState;
    use modelstat_parsers::{FileChange, ParseStats, PrOutcome};
    use modelstat_pipeline::NoEmbedder;
    use modelstat_sumclient::{CompleteRequest, SumError};
    use modelstat_wire::GitContext;
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

    // Stands in for the on-disk spool: records every batch it accepts, or refuses
    // everything when `refuse` is set (a full disk / unwritable spool). `&self`
    // because the real sink takes `&self` too.
    #[derive(Default)]
    struct RecordingSink {
        /// (event_count, raw) per accepted batch.
        accepted: Mutex<Vec<(usize, bool)>>,
        /// `source_event_id`s of EVERY batch offered (refused or accepted) — lets a
        /// test prove a held batch and the one the next cycle rebuilds are
        /// identical, so the server dedupes on id (no duplicates after a crash).
        offered: Mutex<Vec<Vec<String>>>,
        refuse: bool,
    }
    impl RecordingSink {
        fn accepted(&self) -> Vec<(usize, bool)> {
            self.accepted.lock().unwrap().clone()
        }
        fn offered(&self) -> Vec<Vec<String>> {
            self.offered.lock().unwrap().clone()
        }
    }
    impl BatchSink for RecordingSink {
        fn accept(&self, pb: &PreparedBatch) -> Result<(), Hold> {
            self.offered.lock().unwrap().push(
                pb.batch
                    .events
                    .iter()
                    .map(|e| e.source_event_id.clone())
                    .collect(),
            );
            if self.refuse {
                return Err(Hold);
            }
            self.accepted
                .lock()
                .unwrap()
                .push((pb.batch.events.len(), pb.raw));
            Ok(())
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
            seq: None,
            started_at: None,
            first_token_at: None,
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            source_event_id: format!("{session}:{ts}"),
            ts: ts.into(),
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
            content_excerpt: Some("hello world".into()),
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
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
                skipped_kinds: std::collections::BTreeMap::new(),
                session_actors: Default::default(),
                source_file: j.path.clone(),
            })
        }
    }

    /// A parse seam that streams `events` and THEN fails — a file whose readable
    /// prefix is fine and whose tail the parser chokes on.
    fn parse_failing_after(
        events: Vec<RawEvent>,
    ) -> impl Fn(&ScanJob, &mut dyn FnMut(Vec<RawEvent>)) -> std::io::Result<ParseResult>
           + Send
           + Sync
           + Clone
           + 'static {
        move |_j: &ScanJob, emit: &mut dyn FnMut(Vec<RawEvent>)| {
            if !events.is_empty() {
                emit(events.clone());
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "malformed tail",
            ))
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
                skipped_kinds: std::collections::BTreeMap::new(),
                session_actors: Default::default(),
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
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert_eq!(t.files_scanned, 2);
        assert_eq!(t.files_unchanged, 0);
        assert!(!t.held);
        assert!(t.batches_spooled >= 1);
        // Local mode never ships raw.
        assert!(sink.accepted().iter().all(|(_, raw)| !raw));
        // Both files' cursors advanced only after their events committed.
        assert!(cursors.get_cursor("/a.jsonl").is_some());
        assert!(cursors.get_cursor("/b.jsonl").is_some());
    }

    /// A file whose TAIL a parser chokes on must not re-ship its confirmed
    /// prefix, and must not do so again next cycle, and again forever.
    ///
    /// The cursor deliberately does NOT advance on a parse error — nothing may
    /// move past bytes nobody read — so the file re-parses every cycle. That is
    /// correct and it is also the whole cost: the `shipped_below` floor is what
    /// keeps the re-parse from turning into a re-SHIP. This pins both halves,
    /// twice over, because "it re-uploads the same 60 bytes every cycle forever"
    /// is a bug you only see on the second pass.
    #[tokio::test]
    async fn a_parse_error_retries_without_re_shipping_the_confirmed_prefix() {
        let resilient = healthy();
        let mut git = NoGit;
        let sink = RecordingSink::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        cursors.set_cursor("/a.jsonl", confirmed_through(60));
        // Everything the parser gets through sits BELOW the confirmed floor —
        // it is exactly the prefix an earlier scan already uploaded.
        let already_shipped = vec![
            at(ev("s1", "2026-07-16T10:00:00.000Z"), 10),
            at(ev("s1", "2026-07-16T10:00:01.000Z"), 20),
        ];

        for pass in 1..=2 {
            let t = run_scan_over_jobs(
                vec![job("/a.jsonl")],
                "dev1",
                "9.9.9",
                "local",
                opts(Some(12), false),
                &resilient,
                &NoEmbedder,
                &AnsweringRedactor,
                &mut git,
                None,
                parse_failing_after(already_shipped.clone()),
                checksum,
                |e| e,
                &no_exists,
                &no_read,
                &sink,
                &mut cursors,
                &mut obs,
                &Accounts::new(),
            )
            .await;

            assert_eq!(t.files_failed, 1, "pass {pass}: the failure is COUNTED");
            assert!(!t.held, "pass {pass}: a bad file is not a hold");
            assert_eq!(
                t.batches_spooled, 0,
                "pass {pass}: the floor kept the confirmed prefix off the wire"
            );
            assert!(
                sink.offered().is_empty(),
                "pass {pass}: not even offered — re-summarising it is the expensive half"
            );
            assert_eq!(
                cursors.get_cursor("/a.jsonl").map(|c| c.size),
                Some(60),
                "pass {pass}: an errored parse vouches for no new bytes, so the floor holds"
            );
        }
    }

    #[tokio::test]
    async fn skips_a_file_whose_size_and_tail_are_unchanged() {
        let resilient = healthy();
        let mut git = NoGit;
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(vec![ev("s1", "2026-07-16T10:00:00.000Z")]),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert_eq!(t.files_unchanged, 1);
        assert_eq!(t.files_scanned, 0);
        assert_eq!(t.batches_spooled, 0);
        assert!(sink.accepted().is_empty());
    }

    #[tokio::test]
    async fn force_read_all_bypasses_the_unchanged_skip() {
        let resilient = healthy();
        let mut git = NoGit;
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(vec![ev("s1", "2026-07-16T10:00:00.000Z")]),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert_eq!(t.files_scanned, 1);
        assert_eq!(t.files_unchanged, 0);
        assert!(t.batches_spooled >= 1);
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
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(vec![ev("s1", "2026-07-16T10:00:00.000Z")]),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(t.held);
        assert_eq!(t.batches_spooled, 0);
        // Never-drop: the file's cursor stays put so it re-parses next cycle.
        assert!(cursors.get_cursor("/a.jsonl").is_none());
        assert!(sink.accepted().is_empty());
    }

    #[tokio::test]
    async fn upload_hold_leaves_cursors_un_advanced() {
        let resilient = healthy();
        let mut git = NoGit;
        // Engine is healthy (batches build), but every upload holds.
        let sink = RecordingSink {
            refuse: true,
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(vec![ev("s1", "2026-07-16T10:00:00.000Z")]),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(t.held);
        assert_eq!(t.batches_spooled, 0);
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
        let mut sink = RecordingSink {
            refuse: true,
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(events.clone()),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
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
        sink.refuse = false;
        let t2 = run_scan_over_jobs(
            vec![job("/a.jsonl")],
            "dev1",
            "9.9.9",
            "local",
            opts(Some(12), false),
            &resilient,
            &NoEmbedder,
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
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
        assert_eq!(sink.offered().len(), 2, "one held attempt + one resend");
        assert!(!sink.offered()[0].is_empty());
        assert_eq!(
            sink.offered()[0],
            sink.offered()[1],
            "resend must carry byte-identical event ids (server dedupes on id)"
        );
    }

    #[tokio::test]
    async fn file_cap_stops_early_and_sets_more_pending() {
        let resilient = healthy();
        let mut git = NoGit;
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(vec![ev("s1", "2026-07-16T10:00:00.000Z")]),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
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

    /// Files parse AHEAD of the flush loop — bounded. While one file's events
    /// are being flushed (in the real daemon: network round-trips), the next
    /// files' parsers must already be running, and never more than
    /// [`PIPELINE_FILES`] at once — that bound is what keeps memory flat.
    /// Consumption order stays job order, so every cursor still advances.
    #[tokio::test]
    async fn file_parses_overlap_and_respect_the_pipeline_bound() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (running2, peak2) = (running.clone(), peak.clone());
        let parse = move |j: &ScanJob, emit: &mut dyn FnMut(Vec<RawEvent>)| {
            let now = running2.fetch_add(1, Ordering::SeqCst) + 1;
            peak2.fetch_max(now, Ordering::SeqCst);
            // Long enough that the pipeline's parses demonstrably overlap.
            std::thread::sleep(Duration::from_millis(40));
            emit(vec![ev(
                &format!("s{}", j.path),
                "2026-07-16T10:00:00.000Z",
            )]);
            running2.fetch_sub(1, Ordering::SeqCst);
            Ok(ParseResult {
                events: Vec::new(),
                tool_calls: Vec::new(),
                script_contexts: Vec::new(),
                stats: ParseStats::default(),
                skipped_kinds: std::collections::BTreeMap::new(),
                session_actors: Default::default(),
                source_file: j.path.clone(),
            })
        };
        let resilient = healthy();
        let mut git = NoGit;
        let sink = RecordingSink::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let jobs: Vec<ScanJob> = (0..6).map(|i| job(&format!("/f{i}.jsonl"))).collect();
        let t = run_scan_over_jobs(
            jobs,
            "dev1",
            "9.9.9",
            "local",
            opts(Some(12), false),
            &resilient,
            &NoEmbedder,
            &AnsweringRedactor,
            &mut git,
            None,
            parse,
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert_eq!(t.files_scanned, 6);
        assert!(!t.held);
        let peak = peak.load(Ordering::SeqCst);
        assert!(
            peak <= PIPELINE_FILES,
            "the pipeline bound leaked: {peak} parses ran at once"
        );
        assert!(
            peak >= 2,
            "parses never overlapped — the pipeline is not pipelining"
        );
        // Order preserved: every file's cursor advanced, and the spooled events
        // arrive in job order (all six fit one trailing batch).
        for i in 0..6 {
            assert!(cursors.get_cursor(&format!("/f{i}.jsonl")).is_some());
        }
        let ids: Vec<String> = sink.offered().concat();
        let want: Vec<String> = (0..6)
            .map(|i| format!("s/f{i}.jsonl:2026-07-16T10:00:00.000Z"))
            .collect();
        assert_eq!(ids, want, "pipelined parses must not reorder events");
    }

    #[tokio::test]
    async fn a_session_straddling_the_flush_boundary_advances_the_cursor_once() {
        // 1100 events in one file → the buffer hits BATCH_MAX_EVENTS mid-file and
        // ships a first batch (before the file's cursor is queued), then the
        // trailing 100 ship in a second batch that advances the cursor. Proves the
        // straddle: cursor advances only after the LAST batch carrying the file.
        let resilient = healthy();
        let mut git = NoGit;
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(!t.held);
        // Two batches: the 1000-event mid-file flush + the 100-event trailing one.
        assert_eq!(t.batches_spooled, 2);
        assert_eq!(t.events_spooled, 1100);
        assert_eq!(sink.accepted().len(), 2);
        assert_eq!(sink.accepted()[0].0, 1000);
        assert_eq!(sink.accepted()[1].0, 100);
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
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_in_chunks(events, 100),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(!t.held);
        assert_eq!(t.events_spooled, 1100);
        assert_eq!(sink.accepted().len(), 2);
        assert_eq!(sink.accepted()[0].0, 1000);
        assert_eq!(sink.accepted()[1].0, 100);
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
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(!t.held);
        // The file was still fully PARSED (cross-line state intact) — only the two
        // already-confirmed events were dropped before the buffer.
        assert_eq!(t.files_scanned, 1);
        assert_eq!(t.events_spooled, 2);
        assert_eq!(sink.accepted().len(), 1);
        assert_eq!(sink.accepted()[0].0, 2);
        // Exactly the tail, by id — never the re-shipped head.
        assert_eq!(
            sink.offered()[0],
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
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(!t.held);
        assert_eq!(t.events_spooled, 0);
        assert!(sink.accepted().is_empty()); // nothing on the wire
        assert_eq!(cursors.get_cursor("/quiet.jsonl").unwrap().size, 100);
    }

    #[tokio::test]
    async fn a_shrunken_file_re_ships_whole_because_its_offsets_are_meaningless() {
        // Truncated / rewritten in place: recorded offsets no longer point at the
        // same lines, so flooring on them would silently drop real events. Only
        // append-in-place growth (`cs.size >= cur.size`) earns a floor.
        let resilient = healthy();
        let mut git = NoGit;
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert_eq!(t.events_spooled, 2); // both, despite offsets below 500
    }

    #[tokio::test]
    async fn the_eager_force_scan_takes_no_floor() {
        // `force_read_all` exists so a session the daemon already processed
        // re-uploads on demand. A floor would make it ship nothing at all.
        let resilient = healthy();
        let mut git = NoGit;
        let sink = RecordingSink::default();
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
            &AnsweringRedactor,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert_eq!(t.events_spooled, 2);
    }

    /// A LIVE detector, which a cloud test needs: cloud egress fail-closes on a
    /// redactor that is missing OR ineffective, and `redactor_active` decides which by
    /// probing with a sentinel name and checking it really disappeared. So this
    /// answers the probe and finds nothing in the test turns themselves.
    struct LiveRedactor;
    impl PiiModel for LiveRedactor {
        fn classify(&self, text: &str) -> Option<Vec<modelstat_redact::PiiToken>> {
            let mut out = Vec::new();
            if let Some(i) = text.find("Katherine Johnson") {
                let tok =
                    |entity: &str, word: &str, a: usize, b: usize| modelstat_redact::PiiToken {
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

    /// Refuses one named session's batch — the "spool would not take it" case,
    /// chosen by NAME so the test does not depend on flush ordering.
    #[derive(Default)]
    struct RefusesOneSession {
        refuse_session: Option<&'static str>,
        accepted: Mutex<Vec<String>>,
    }
    impl BatchSink for RefusesOneSession {
        fn accept(&self, pb: &PreparedBatch) -> Result<(), Hold> {
            let session = pb.batch.events.first().map(|e| e.session_id.as_str());
            if self.refuse_session.is_some() && self.refuse_session == session {
                return Err(Hold);
            }
            self.accepted
                .lock()
                .unwrap()
                .push(session.unwrap_or_default().to_string());
            Ok(())
        }
    }

    /// Never-drop, at the new boundary: if ANY batch in a flush cannot be parked,
    /// no cursor from that flush advances — even though its siblings were parked
    /// fine. The whole file re-parses next cycle and the server dedupes the
    /// resends by event id.
    ///
    /// This is the property the old concurrent-upload "prefix rule" protected. It
    /// still has to hold; only the failure it guards against has changed, from "the
    /// server would not take it" to "this disk would not take it".
    #[tokio::test]
    async fn a_refusal_anywhere_in_the_flush_advances_no_cursor() {
        let resilient = healthy();
        let mut git = NoGit;
        let sink = RefusesOneSession {
            refuse_session: Some("s2"),
            ..Default::default()
        };
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        // Cloud mode ⇒ one raw batch per session, so one flush yields three.
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
            &LiveRedactor,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(t.held, "one refused batch holds the flush");
        assert!(
            cursors.get_cursor("/three.jsonl").is_none(),
            "never-drop: a refusal anywhere in the flush advances NO cursor"
        );
        assert!(
            t.batches_spooled < 3,
            "the tally stops at the first refusal, counted {}",
            t.batches_spooled
        );
    }

    /// Counts how many times the PII model is asked to classify anything — the
    /// expensive work, and the thing this whole change exists to stop repeating.
    #[derive(Default)]
    struct CountingRedactor {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl PiiModel for CountingRedactor {
        fn classify(&self, text: &str) -> Option<Vec<modelstat_redact::PiiToken>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut out = Vec::new();
            if let Some(i) = text.find("Katherine Johnson") {
                let tok =
                    |entity: &str, word: &str, a: usize, b: usize| modelstat_redact::PiiToken {
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

    /// THE regression test for the bug this change fixes.
    ///
    /// A user reported modelstat pinning their CPU. The cause was that redaction
    /// and uploading shared one cursor: a flush that could not commit threw the
    /// redacted batch away, so the next cycle re-read the same transcript and ran
    /// the PII model over the same turns again — every five minutes, for as long
    /// as the server was unreachable. Their log had 77 held flushes in one
    /// morning, each one paying for redaction it then burned.
    ///
    /// The fix is that the scan's cursor now tracks DURABILITY, not delivery. So
    /// with a real spool that nobody ever drains — the permanent-outage case — the
    /// model must still be asked exactly once per turn, no matter how many scan
    /// cycles go by.
    #[tokio::test]
    async fn an_outage_never_makes_the_redactor_run_twice() {
        let dir = std::env::temp_dir().join(format!("modelstat-noredo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let spool = crate::spool::Spool::open(&dir, crate::spool::DEFAULT_MAX_SPOOL_BYTES).unwrap();
        let resilient = healthy();
        let mut git = NoGit;
        let redactor = CountingRedactor::default();
        let mut cursors = RuntimeState::default();
        let mut obs = ();
        let events = vec![
            ev("s1", "2026-07-16T10:00:00.000Z"),
            ev("s1", "2026-07-16T10:01:00.000Z"),
        ];

        // A macro rather than a closure: a closure returning a future that borrows
        // `cursors` cannot name the lifetime, and the error lands far from here.
        macro_rules! scan_once {
            () => {
                run_scan_over_jobs(
                    vec![job("/a.jsonl")],
                    "dev1",
                    "9.9.9",
                    "cloud",
                    opts(Some(12), false),
                    &resilient,
                    &NoEmbedder,
                    &redactor,
                    &mut git,
                    None,
                    parse_with(events.clone()),
                    checksum,
                    |e| e,
                    &no_exists,
                    &no_read,
                    &spool,
                    &mut cursors,
                    &mut obs,
                    &Accounts::new(),
                )
                .await
            };
        }

        let first = scan_once!();
        assert!(!first.held, "parking a batch must always succeed");
        assert_eq!(first.batches_spooled, 1);
        let after_first = redactor.calls.load(std::sync::atomic::Ordering::SeqCst);
        assert!(after_first > 0, "the first scan must actually redact");
        assert!(
            cursors.get_cursor("/a.jsonl").is_some(),
            "a durably parked batch advances the cursor even though nothing was sent"
        );

        // The server stays down: the spool is never drained, so the batch is still
        // sitting there. Four more cycles go by.
        for _ in 0..4 {
            let t = scan_once!();
            assert_eq!(t.files_unchanged, 1, "an unchanged file must be skipped");
            assert_eq!(t.batches_spooled, 0);
        }
        assert_eq!(
            redactor.calls.load(std::sync::atomic::Ordering::SeqCst),
            after_first,
            "five scan cycles through an outage must cost ONE redaction pass — \
             this counter growing IS the CPU bug"
        );

        // And the work really is still there, whole, waiting for the wire.
        assert_eq!(spool.list().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cloud mode splits one flush into a batch per session, and every one of them
    /// must reach the sink — this is what the uploader later drains concurrently.
    #[tokio::test]
    async fn every_session_in_a_flush_reaches_the_sink() {
        let resilient = healthy();
        let mut git = NoGit;
        let sink = RefusesOneSession::default();
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
            &LiveRedactor,
            &mut git,
            None,
            parse_with(events),
            checksum,
            |e| e,
            &no_exists,
            &no_read,
            &sink,
            &mut cursors,
            &mut obs,
            &Accounts::new(),
        )
        .await;

        assert!(!t.held);
        assert_eq!(t.batches_spooled, 3, "one batch per session");
        assert_eq!(t.events_spooled, 3);
        assert_eq!(sink.accepted.lock().unwrap().len(), 3);
        assert!(cursors.get_cursor("/three.jsonl").is_some());
    }
}
