//! Batch flush — the composition at the heart of `apps/daemon/src/scan.ts`'s
//! `flushBatch`, lifted out of the scan I/O so it's pure + testable with a fake
//! engine. Given a buffer of git-corrected events + tool-call drafts, it produces
//! the `IngestBatch`(es) to ship — or holds.
//!
//! Two paths, keyed on the summariser mode:
//!   - **cloud**: redact the raw turns on-device ([`prepare_cloud_raw_events`],
//!     FAIL-CLOSED) and emit ONE raw batch per session (no local segments — the
//!     server summarises). Detector down ⇒ [`FlushOutcome::Held`] (no-degrade: we hold
//!     + retry rather than ship floor-only-redacted turns, §21.5).
//!   - **local / self-hosted**: [`build_for_one_session`] per session (any
//!     `Held` ⇒ hold the whole flush, never a partial batch), then titles +
//!     metadata + segment attribution → one `IngestBatch`.
//!
//! The scan loop owns the I/O: it uploads each [`PreparedBatch`] and advances the
//! buffered files' cursors only on a confirmed commit (never-drop).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use modelstat_ingest::accounts::{session_installs_for, Accounts};
use modelstat_parsers::{
    detect_references, DetectedRefs, GitEnrichment, SessionActors, ToolCallDraft,
};
use modelstat_pipeline::{
    attach_segment_ids_by_map, batch_id, build_for_one_session, build_session_metadata,
    build_session_titles, deep_redact_tool_commands, enrich_tool_call_redaction,
    prepare_cloud_raw_events, BuildOutcome, Embedder, LinkExtractor, ResilientSummarizer,
    Summarizer,
};
use modelstat_redact::PiiModel;
use modelstat_wire::{IngestBatch, RawEvent, Segment, SegmentGeneration, TokenUsage};

/// One assembled batch plus how the scan loop should ship it: `raw` picks the
/// `/v1/ingest/raw` endpoint (cloud), `segment_count` is what the tray surfaces.
pub struct PreparedBatch {
    pub batch: IngestBatch,
    pub raw: bool,
    pub segment_count: usize,
}

/// The outcome of a flush: batches ready to ship (possibly empty), or a hold.
pub enum FlushOutcome {
    Ready(Vec<PreparedBatch>),
    Held,
}

/// The `session_id -> [actor, …]` blob for the sessions a batch ships.
///
/// Registry rows are held keyed by id while they are being folded together;
/// the wire wants a list, and the conversion happens once, here, at the door.
/// A session nothing was recorded for stays ABSENT rather than riding as an
/// empty list — an empty list is a claim that the session ran no sub-agents,
/// and the honest answer is that this scan saw none.
fn actors_wire_for<'a>(
    run_actors: &SessionActors,
    sessions: impl IntoIterator<Item = &'a str>,
) -> Option<serde_json::Value> {
    let mut out: BTreeMap<&str, Vec<&modelstat_parsers::SessionActor>> = BTreeMap::new();
    for sid in sessions {
        if let Some(actors) = run_actors.get(sid) {
            if !actors.is_empty() {
                out.insert(sid, actors.values().collect());
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    serde_json::to_value(&out).ok()
}

/// Zero out null token usage — the ingest server rejects null `tokens`, and this
/// runs in every mode (cloud ships these events too). Port of `withNonNullTokens`.
pub fn with_non_null_tokens(mut e: RawEvent) -> RawEvent {
    if e.tokens.is_none() {
        e.tokens = Some(TokenUsage::default());
    }
    e
}

/// Assemble the batches for one flush. `events` are already authoritative-git
/// corrected; token normalisation happens here. `run_segments_by_session` is the
/// scan-long accumulator (shared across flushes) that titles + call-attribution
/// read so a session split across flush boundaries keeps its full-view title and
/// its straddling calls stay attributed.
#[allow(clippy::too_many_arguments)]
/// One scan's identity, and which sessions it may claim to have restated
/// (core#701).
///
/// Supersession on the server used to be inferred from TIME OVERLAP, which
/// cannot work: the scan flushes every `BATCH_MAX_EVENTS`, so one session's
/// segmentation leaves in several batches, and a cursor-resumed scan overlaps
/// older segments without re-stating them. The server retired those anyway and
/// 116 sessions — 29.5% of all measured work — lost their segments entirely.
///
/// Only the scan knows which it did, so the scan says.
#[derive(Debug, Clone, Default)]
pub struct ScanGeneration {
    /// Stable for the whole scan and increasing between scans. Every batch of
    /// one scan carries it, which is what lets the server retire the generation
    /// it replaces without ever retiring the scan's own other batches.
    pub id: String,
    /// Sessions this scan read from byte 0. Everything else appends.
    pub read_whole: BTreeSet<String>,
}

impl ScanGeneration {
    /// The per-session claim for a batch carrying `batch_segments`.
    ///
    /// The replaced span is taken from the RUN's segments for that session, not
    /// the batch's, so a batch that carries the middle of a generation still
    /// names the generation's span rather than its own slice. Sessions this
    /// scan only appended to get an id and NO span: the server then retires
    /// nothing for them, because what it did not re-read is still the truth.
    #[must_use]
    pub fn claims(
        &self,
        batch_segments: &[Segment],
        run_segments: &BTreeMap<String, Vec<Segment>>,
    ) -> Option<BTreeMap<String, SegmentGeneration>> {
        if self.id.is_empty() {
            return None;
        }
        let mut out: BTreeMap<String, SegmentGeneration> = BTreeMap::new();
        for sid in batch_segments.iter().map(|s| s.session_id.as_str()) {
            if out.contains_key(sid) {
                continue;
            }
            let span = if self.read_whole.contains(sid) {
                run_segments.get(sid).and_then(|segs| {
                    let from = segs.iter().map(|s| s.started_at.as_str()).min()?;
                    let to = segs.iter().map(|s| s.ended_at.as_str()).max()?;
                    Some((from.to_string(), to.to_string()))
                })
            } else {
                None
            };
            out.insert(
                sid.to_string(),
                SegmentGeneration {
                    id: self.id.clone(),
                    replaces_from: span.as_ref().map(|(f, _)| f.clone()),
                    replaces_to: span.as_ref().map(|(_, t)| t.clone()),
                },
            );
        }
        (!out.is_empty()).then_some(out)
    }
}

pub async fn build_flush_batches<S, E, N>(
    device_id: &str,
    daemon_version: &str,
    mode: &str,
    events: Vec<RawEvent>,
    mut drafts: Vec<ToolCallDraft>,
    resilient: &ResilientSummarizer<S>,
    embedder: &E,
    redactor: &N,
    // Moved into the metadata pass (its only use), so no reborrow lifetime dance.
    // `+ Send`: this runs inside the daemon's tokio-spawned single-flight scan,
    // whose future must be `Send` (a `dyn Trait` erases the concrete Send-ness).
    git: Option<&mut (dyn GitEnrichment + Send)>,
    extract_links: Option<&LinkExtractor<'_>>,
    run_segments_by_session: &mut BTreeMap<String, Vec<Segment>>,
    // The cloud twin of `run_segments_by_session`: cloud ships no local segments,
    // so the run-long view a session's metadata is recomputed from has to be its
    // turns instead (excerpt-shed — see the accumulation below). Untouched in
    // local / self-hosted mode, which reads the segment map.
    run_events_by_session: &mut BTreeMap<String, Vec<RawEvent>>,
    // Every agent-instance the scan has seen each session state, accumulated
    // across files and flushes for the same reason the two maps above are: a
    // session's sub-agents are discovered over many files (each one is its own
    // transcript) and the server takes the last write, so a flush that carried
    // only the actors IT happened to read would narrow what an earlier flush
    // already landed.
    run_actors_by_session: &SessionActors,
    // This scan's identity, and which sessions it re-read WHOLE (core#701).
    //
    // A session in the set was parsed from byte 0, so the segments accumulated
    // for it in `run_segments_by_session` restate its whole span and the server
    // may retire what they replace. A session absent from it was resumed at a
    // cursor: this batch appends, claims no span, and the server retires
    // nothing — which is the bug that lost 29.5% of all measured work.
    generation: &ScanGeneration,
    // Which provider account is logged in, and since when. Passed in rather than
    // read here so the caller owns the I/O and this stays a pure builder.
    accounts: &Accounts,
) -> FlushOutcome
where
    S: Summarizer,
    E: Embedder,
    N: PiiModel,
{
    if events.is_empty() && drafts.is_empty() {
        return FlushOutcome::Ready(Vec::new());
    }
    let events: Vec<RawEvent> = events.into_iter().map(with_non_null_tokens).collect();

    // ── Cloud: redact on-device, ship raw, summarise server-side ──
    if mode == "cloud" {
        let Some(cloud_events) = prepare_cloud_raw_events(&events, &mut drafts, redactor) else {
            // FAIL-CLOSED + no-degrade: the PII redactor cannot answer,
            // so turns can't be scrubbed before leaving the box. HOLD + retry (the
            // TS shipped local extractive; the rewrite never degrades).
            modelstat_log::log_warn!(
                "the PII redactor cannot answer — holding this flush \
                 (no raw egress, no degrade); retrying once it is ready"
            );
            return FlushOutcome::Held;
        };
        // Accumulate this flush's turns into the run-long per-session metadata
        // view, the cloud twin of `run_segments_by_session` below. A session's
        // turns arrive over MANY flushes (the 1000-event buffer spans files, and
        // a live session ships only its new turns each cycle), and the server
        // versions `session_metadata` by timestamp — last write wins. So a flush
        // that computed metadata from its own tail alone would OVERWRITE the
        // richer answer an earlier flush already landed. Recomputing from the
        // full run view every flush makes the newest write the most complete one.
        //
        // Stored as a projection, not the turn: the content channel is mined ONCE
        // here into `references` and the excerpt dropped. That keeps the map to
        // ids + cwd + git + a small refs blob for the whole scan (the same
        // rationale as shedding the embedding below), and keeps the per-flush
        // recompute off the regex path it would otherwise re-walk every time.
        for e in &events {
            let mut lite = e.clone();
            if let Some(excerpt) = lite.content_excerpt.take().filter(|x| !x.is_empty()) {
                let mut refs = lite
                    .references
                    .take()
                    .and_then(|r| serde_json::from_value::<DetectedRefs>(r).ok())
                    .unwrap_or_default();
                let mined = detect_references(&excerpt, "content");
                refs.repos.extend(mined.repos);
                refs.pull_requests.extend(mined.pull_requests);
                refs.issues.extend(mined.issues);
                lite.references = serde_json::to_value(&refs).ok();
            }
            run_events_by_session
                .entry(e.session_id.clone())
                .or_default()
                .push(lite);
        }
        // ONE session per /v1/ingest/raw request (the unit of send = unit of retry).
        let wire_calls = attach_segment_ids_by_map(&drafts, &HashMap::new());
        let mut by_session: BTreeMap<String, (Vec<RawEvent>, Vec<_>)> = BTreeMap::new();
        for e in cloud_events {
            by_session
                .entry(e.session_id.clone())
                .or_default()
                .0
                .push(e);
        }
        for c in wire_calls {
            by_session
                .entry(c.session_id.clone())
                .or_default()
                .1
                .push(c);
        }
        // The join layer between AI spend and shipped work rides the raw path too.
        // Only public reference shapes leave the box (slugs, PR/issue numbers,
        // repo-relative paths — never prompt or code text), so this is strictly
        // less egress than the turns already shipped beside it. Restricted to the
        // sessions this flush ships; no local segments exist in cloud mode, so the
        // pass runs off the accumulated events alone.
        let mut metadata_input: Vec<RawEvent> = Vec::new();
        for sid in by_session.keys() {
            if let Some(evs) = run_events_by_session.get(sid) {
                metadata_input.extend(evs.iter().cloned());
            }
        }
        let session_metadata =
            build_session_metadata(&[], &metadata_input, git, extract_links).await;
        let out = by_session
            .into_iter()
            .map(|(sid, (evs, calls))| {
                // Computed before the struct literal below moves `evs`.
                let installs_for_session = session_installs_for(&evs, accounts);
                let actors_for_session = actors_wire_for(run_actors_by_session, [sid.as_str()]);
                PreparedBatch {
                    batch: IngestBatch {
                        batch_id: batch_id(),
                        device_id: device_id.into(),
                        daemon_version: daemon_version.into(),
                        events: evs,
                        segments: Vec::new(),
                        tool_calls: calls,
                        // The account that produced this session, when we can say so
                        // honestly (see `accounts::session_installs_for`). One session
                        // per raw batch, so only its own entry rides.
                        session_installs: installs_for_session,
                        session_actors: None,
                        session_titles: None,
                        // One session per batch ⇒ ship that session's entry alone. A
                        // session the pass found nothing for stays absent rather than
                        // riding as an empty map, which would only overwrite better
                        // server state (`build_session_metadata` omits empty ones).
                        session_metadata: session_metadata
                            .get(&sid)
                            .and_then(|m| serde_json::to_value(BTreeMap::from([(&sid, m)])).ok()),
                        summarizer_mode: None,
                        redactor_mode: None,
                        repo_anchors: None,
                        // Cloud ships no local segments, so there is no generation to
                        // supersede (core#701).
                        segment_generations: None,
                    },
                    raw: true,
                    segment_count: 0,
                }
            })
            .collect();
        return FlushOutcome::Ready(out);
    }

    // ── Local / self-hosted: summarise, one-or-more Segments per session ──
    // Any session that HELD (engine down) holds the WHOLE flush — never a partial
    // batch, which would advance the cursor past un-summarised events (§21.2).
    let mut segments: Vec<Segment> = Vec::new();
    let mut by_session: BTreeMap<String, Vec<RawEvent>> = BTreeMap::new();
    for e in &events {
        by_session
            .entry(e.session_id.clone())
            .or_default()
            .push(e.clone());
    }
    for sess_events in by_session.values() {
        match build_for_one_session(sess_events, resilient, embedder, redactor).await {
            BuildOutcome::Held => return FlushOutcome::Held,
            BuildOutcome::Ready(segs) => segments.extend(segs),
        }
    }

    // Accumulate into the run-long map, shedding the 384-float embedding (titling
    // + metadata + attribution only read the abstract + ids, and this map is held
    // for the whole scan — retaining embeddings OOM'd on a full re-scan).
    for seg in &segments {
        let mut lite = seg.clone();
        lite.abstract_embedding = None;
        run_segments_by_session
            .entry(seg.session_id.clone())
            .or_default()
            .push(lite);
    }

    // Title + metadata use the FULL run-accumulated view of each session in this
    // batch (last-write-wins server-side, so a partial-view title must not win).
    let sessions_in_batch: BTreeSet<&str> =
        segments.iter().map(|s| s.session_id.as_str()).collect();
    let mut title_input: Vec<Segment> = Vec::new();
    for sid in &sessions_in_batch {
        if let Some(segs) = run_segments_by_session.get(*sid) {
            title_input.extend(segs.iter().cloned());
        }
    }
    // Auxiliary + best-effort: a titler/detector hiccup never sinks a batch.
    let session_titles = build_session_titles(&title_input, resilient.engine()).await;
    let session_metadata = build_session_metadata(&title_input, &events, git, extract_links).await;

    // Deep-redact the SHIPPED tool commands before attribution: L2 (the PII detector, always
    // on-device) + L3 (LLM backstop, LOCAL mode only — §21.13, never crosses the
    // machine boundary). Both fail-safe: a down detector/engine leaves the L1-floored
    // command unchanged. The cloud path already ran L2 inside prepare_cloud_raw_events.
    enrich_tool_call_redaction(&mut drafts, redactor);
    if mode == "local" {
        deep_redact_tool_commands(&mut drafts, resilient.engine()).await;
    }

    // Attribute each buffered call to the segment covering its source event,
    // resolved against every segment seen this run for the call's session.
    let mut call_seg_by_event: HashMap<String, String> = HashMap::new();
    let sessions_in_calls: BTreeSet<&str> = drafts.iter().map(|c| c.session_id.as_str()).collect();
    for sid in &sessions_in_calls {
        if let Some(segs) = run_segments_by_session.get(*sid) {
            for seg in segs {
                for id in &seg.source_event_ids {
                    call_seg_by_event.insert(id.clone(), seg.segment_id.clone());
                }
            }
        }
    }
    let tool_calls = attach_segment_ids_by_map(&drafts, &call_seg_by_event);

    let segment_count = segments.len();
    let session_metadata_value = if session_metadata.is_empty() {
        None
    } else {
        serde_json::to_value(&session_metadata).ok()
    };
    // Computed before the struct literal below moves `events`.
    let session_installs = session_installs_for(&events, accounts);
    let session_actors = actors_wire_for(
        run_actors_by_session,
        events.iter().map(|e| e.session_id.as_str()),
    );
    // The claim, per session this batch carries segments for. The span comes
    // from the RUN's accumulated segments, not this batch's: a scan's segments
    // leave across several batches, so an early batch would otherwise claim a
    // span narrower than the generation it is part of. Narrower is the safe
    // direction — it retires less, and later batches of the same scan widen it —
    // whereas over-claiming is what over-retired in core#698.
    let segment_generations = generation.claims(&segments, run_segments_by_session);
    let batch = IngestBatch {
        batch_id: batch_id(),
        device_id: device_id.into(),
        daemon_version: daemon_version.into(),
        events,
        segments,
        tool_calls,
        // The account behind each session in this batch, for the sessions we can
        // name one for; absent entirely when we can name none.
        session_installs,
        session_actors,
        session_titles: if session_titles.is_empty() {
            None
        } else {
            Some(session_titles)
        },
        session_metadata: session_metadata_value,
        summarizer_mode: None,
        redactor_mode: None,
        repo_anchors: None,
        segment_generations,
    };
    FlushOutcome::Ready(vec![PreparedBatch {
        batch,
        raw: false,
        segment_count,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// core#701. A scan that re-read a session WHOLE may say what span it
    /// restates; one that resumed at a cursor may not, and the server then
    /// retires nothing for it. Getting that backwards is what left 116 sessions
    /// — 29.5% of all measured work — with no live segment at all.
    #[test]
    fn only_a_whole_read_claims_a_span() {
        let seg = |sid: &str, from: &str, to: &str| Segment {
            segment_id: format!("seg_{sid}_{from}"),
            session_id: sid.into(),
            agent: "claude_code".into(),
            started_at: from.into(),
            ended_at: to.into(),
            r#abstract: String::new(),
            tokens: TokenUsage::default(),
            tags: Vec::new(),
            redaction: Default::default(),
            source_event_ids: Vec::new(),
            abstract_embedding: None,
            behavior: None,
            user_intent: None,
            local_time: None,
        };
        // The run saw more of the session than this batch carries: the claim
        // must name the RUN's span, or an early batch under-claims and a later
        // one would have to re-retire.
        let run: BTreeMap<String, Vec<Segment>> = [(
            "s1".to_string(),
            vec![
                seg("s1", "2026-06-10T10:00:00Z", "2026-06-10T10:10:00Z"),
                seg("s1", "2026-06-10T10:20:00Z", "2026-06-10T10:30:00Z"),
            ],
        )]
        .into_iter()
        .collect();
        let batch = vec![seg("s1", "2026-06-10T10:00:00Z", "2026-06-10T10:10:00Z")];

        let whole = ScanGeneration {
            id: "scan_1".into(),
            read_whole: ["s1".to_string()].into_iter().collect(),
        };
        let claims = whole.claims(&batch, &run).expect("a claim");
        let c = &claims["s1"];
        assert_eq!(c.id, "scan_1");
        assert_eq!(c.replaces_from.as_deref(), Some("2026-06-10T10:00:00Z"));
        assert_eq!(
            c.replaces_to.as_deref(),
            Some("2026-06-10T10:30:00Z"),
            "the RUN's span, not this batch's slice"
        );

        let appended = ScanGeneration {
            id: "scan_1".into(),
            read_whole: Default::default(),
        };
        let claims = appended.claims(&batch, &run).expect("a claim");
        let c = &claims["s1"];
        assert_eq!(c.id, "scan_1", "an append still names its scan");
        assert!(
            c.replaces_from.is_none() && c.replaces_to.is_none(),
            "an append restates nothing, so it claims no span"
        );

        // No scan id at all ⇒ no claim, and the server falls back to its old
        // rule rather than reading an empty id as a generation.
        assert!(ScanGeneration::default().claims(&batch, &run).is_none());
    }

    use super::*;
    use crate::testing::AnsweringRedactor;
    use modelstat_pipeline::NoEmbedder;
    use modelstat_redact::UnavailableRedactor;
    use modelstat_sumclient::CompleteRequest;
    use std::time::Duration;

    // A fake engine: healthy → a fixed reply for every pass; failing → errors so
    // the resilient wrapper reports Held.
    struct Fake {
        reply: String,
        failing: bool,
    }
    impl Summarizer for Fake {
        async fn complete(
            &self,
            _req: &CompleteRequest,
        ) -> Result<String, modelstat_sumclient::SumError> {
            if self.failing {
                Err(modelstat_sumclient::SumError::Http(503))
            } else {
                Ok(self.reply.clone())
            }
        }
    }

    /// A fake "live" detector — tags Katherine Johnson / Globex Corporation by surface,
    /// so `redactor_active`'s sentinel scrubs and the cloud path proceeds instead of
    /// fail-closing. Cloud tests that want the HOLD use `UnavailableRedactor`.
    struct LiveRedactor;
    impl modelstat_redact::PiiModel for LiveRedactor {
        fn classify(&self, _text: &str) -> Option<Vec<modelstat_redact::PiiToken>> {
            let tok = |ent: &str, word: &str| modelstat_redact::PiiToken {
                entity: ent.into(),
                word: word.into(),
                start: None,
                end: None,
            };
            Some(vec![
                tok("B-PER", "Katherine"),
                tok("I-PER", "Johnson"),
                tok("B-ORG", "Globex"),
                tok("I-ORG", "Corporation"),
            ])
        }
    }

    fn ev(session: &str, ts: &str, excerpt: &str) -> RawEvent {
        RawEvent {
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
            content_excerpt: Some(excerpt.into()),
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    /// The wiring, end to end: a known account reaches the wire field the server
    /// reads. The decision logic itself is unit-tested in
    /// `modelstat_ingest::accounts`; this proves the batch actually carries it.
    #[tokio::test]
    async fn a_local_flush_names_the_logged_in_account_on_the_wire() {
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "Did the thing".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        // Logged in well before the session ran.
        let accounts = Accounts::from([(
            "anthropic".to_string(),
            modelstat_ingest::accounts::AccountSnapshot {
                provider_account_id: "acct-uuid-1".into(),
                observed_since: 0,
            },
        )]);
        let events = vec![
            ev("s1", "2026-07-16T10:00:00.000Z", "hello"),
            ev("s1", "2026-07-16T10:01:00.000Z", "world"),
        ];
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "local",
            events,
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &AnsweringRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &accounts,
        )
        .await;
        let FlushOutcome::Ready(batches) = out else {
            panic!("healthy engine must not hold");
        };
        let installs = batches[0]
            .batch
            .session_installs
            .as_ref()
            .expect("the account was known — it must ride on the wire");
        assert_eq!(
            installs["s1"]["provider_account_id"], "acct-uuid-1",
            "the VENDOR's id, which the server translates into ours"
        );
        assert_eq!(installs["s1"]["provider"], "anthropic");
    }

    /// The install-day backlog: months of transcripts, account first seen today.
    /// Naming one here would be a guess, and a guess puts money on the wrong
    /// account.
    #[tokio::test]
    async fn a_local_flush_names_nothing_for_sessions_older_than_the_account() {
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "Did the thing".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        // First seen LONG after the session's events (2026-07-16 ≈ 1.784e12 ms).
        let accounts = Accounts::from([(
            "anthropic".to_string(),
            modelstat_ingest::accounts::AccountSnapshot {
                provider_account_id: "acct-uuid-1".into(),
                observed_since: 9_000_000_000_000,
            },
        )]);
        let events = vec![ev("s1", "2026-07-16T10:00:00.000Z", "hello")];
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "local",
            events,
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &AnsweringRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &accounts,
        )
        .await;
        let FlushOutcome::Ready(batches) = out else {
            panic!("healthy engine must not hold");
        };
        assert!(
            batches[0].batch.session_installs.is_none(),
            "an unnameable session must leave the field OFF the wire, not send an empty map"
        );
    }

    #[tokio::test]
    async fn local_flush_builds_a_batch_with_segments() {
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "Did the thing".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        let events = vec![
            ev("s1", "2026-07-16T10:00:00.000Z", "hello"),
            ev("s1", "2026-07-16T10:01:00.000Z", "world"),
        ];
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "local",
            events,
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &AnsweringRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &Accounts::new(),
        )
        .await;
        match out {
            FlushOutcome::Ready(batches) => {
                assert_eq!(batches.len(), 1);
                assert!(!batches[0].raw);
                assert!(!batches[0].batch.segments.is_empty());
                assert_eq!(batches[0].batch.device_id, "dev1");
                // Null tokens were zeroed for the wire.
                assert!(batches[0].batch.events.iter().all(|e| e.tokens.is_some()));
            }
            FlushOutcome::Held => panic!("healthy engine must not hold"),
        }
    }

    #[tokio::test]
    async fn engine_down_holds_the_whole_flush() {
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: String::new(),
                failing: true,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        let events = vec![ev("s1", "2026-07-16T10:00:00.000Z", "hello")];
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "local",
            events,
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &AnsweringRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &Accounts::new(),
        )
        .await;
        assert!(matches!(out, FlushOutcome::Held));
    }

    #[tokio::test]
    async fn cloud_mode_holds_when_the_redactor_is_down() {
        // Fail-closed: cloud + UnavailableRedactor → Held (no raw egress, no degrade).
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "x".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        let events = vec![ev("s1", "2026-07-16T10:00:00.000Z", "secret stuff")];
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "cloud",
            events,
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &UnavailableRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &Accounts::new(),
        )
        .await;
        assert!(matches!(out, FlushOutcome::Held));
    }

    /// The `{session_id: {repos,pull_requests,...}}` blob a raw batch carries.
    fn meta_of(pb: &PreparedBatch) -> serde_json::Value {
        pb.batch
            .session_metadata
            .clone()
            .expect("the cloud batch must carry session_metadata")
    }

    #[tokio::test]
    async fn cloud_flush_ships_session_metadata() {
        // The regression this fixes: cloud is the DEFAULT mode, and it used to
        // hardcode `session_metadata: None`, so the table was empty in production
        // for every default install.
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "x".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        let events = vec![ev(
            "s1",
            "2026-07-16T10:00:00.000Z",
            "fixed it in https://github.com/acme/web/pull/42",
        )];
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "cloud",
            events,
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &LiveRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &Accounts::new(),
        )
        .await;
        let FlushOutcome::Ready(batches) = out else {
            panic!("a live detector must not hold")
        };
        assert_eq!(batches.len(), 1, "one session ⇒ one raw batch");
        assert!(batches[0].raw, "cloud ships the raw endpoint");
        let meta = meta_of(&batches[0]);
        assert_eq!(
            meta["s1"]["pull_requests"][0]["number"], 42,
            "the PR the turn referenced must ride the batch, keyed by session"
        );
    }

    #[tokio::test]
    async fn cloud_metadata_is_recomputed_from_the_whole_run_not_one_flush() {
        // A session's turns arrive over many flushes and the server's last write
        // wins, so the LATER flush has to carry the earlier flush's references
        // too — otherwise it overwrites them with its own narrower view.
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "x".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        let first = build_flush_batches(
            "dev1",
            "9.9.9",
            "cloud",
            vec![ev(
                "s1",
                "2026-07-16T10:00:00.000Z",
                "opened https://github.com/acme/web/pull/42",
            )],
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &LiveRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &Accounts::new(),
        )
        .await;
        let FlushOutcome::Ready(first) = first else {
            panic!("ready")
        };
        assert_eq!(meta_of(&first[0])["s1"]["pull_requests"][0]["number"], 42);

        // The same session continues; this flush's turns mention nothing.
        let second = build_flush_batches(
            "dev1",
            "9.9.9",
            "cloud",
            vec![ev("s1", "2026-07-16T10:05:00.000Z", "thanks, looks good")],
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &LiveRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &Accounts::new(),
        )
        .await;
        let FlushOutcome::Ready(second) = second else {
            panic!("ready")
        };
        assert_eq!(
            meta_of(&second[0])["s1"]["pull_requests"][0]["number"],
            42,
            "the later flush must still carry PR 42 — it wins on version, so a \
             narrower view here would erase the reference server-side"
        );
    }

    #[tokio::test]
    async fn cloud_batch_omits_metadata_when_no_references_are_found() {
        // An empty map would overwrite better server state, so it must stay absent.
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "x".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "cloud",
            vec![ev("s1", "2026-07-16T10:00:00.000Z", "no refs here at all")],
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &LiveRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &Accounts::new(),
        )
        .await;
        let FlushOutcome::Ready(batches) = out else {
            panic!("ready")
        };
        assert!(batches[0].batch.session_metadata.is_none());
    }

    /// The registry reaches the wire, in the shape the zod mirror validates —
    /// and a batch built through the real path serializes to it. The two halves
    /// (an event's `actor_id`, the roster it joins against) are produced by
    /// different code, so nothing but a test keeps them meeting.
    #[tokio::test]
    async fn a_flush_ships_the_actor_roster_its_events_point_at() {
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "Did the thing".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        let mut actors = SessionActors::new();
        modelstat_parsers::record_actor(
            &mut actors,
            "s1",
            modelstat_parsers::SessionActor {
                id: "a0123456789abcdef".into(),
                label: Some("Explore".into()),
                description: Some("Audit the alerting dashboards".into()),
                spawn_depth: Some(1),
                first_ts: Some("2026-07-16T10:00:00.000Z".into()),
                last_ts: Some("2026-07-16T10:00:00.000Z".into()),
                ..Default::default()
            },
        );
        // A session this flush does not ship must not leak into the batch.
        modelstat_parsers::record_actor(
            &mut actors,
            "s_elsewhere",
            modelstat_parsers::SessionActor {
                id: "affffffffffffffff".into(),
                ..Default::default()
            },
        );
        let mut event = ev("s1", "2026-07-16T10:00:00.000Z", "hello");
        event.actor_id = Some("a0123456789abcdef".into());
        event.reasoning_excerpt = Some("thinking about it".into());
        event.reasoning_bytes = Some(17);
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "local",
            vec![event],
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &AnsweringRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &actors,
            &ScanGeneration::default(),
            &Accounts::new(),
        )
        .await;
        let FlushOutcome::Ready(batches) = out else {
            panic!("healthy engine must not hold");
        };
        let json = serde_json::to_value(&batches[0].batch).unwrap();
        let roster = &json["session_actors"]["s1"];
        assert_eq!(roster[0]["id"], "a0123456789abcdef");
        assert_eq!(roster[0]["label"], "Explore");
        assert_eq!(roster[0]["spawn_depth"], 1);
        assert!(
            roster[0].get("path").is_none(),
            "a key the harness never stated must be ABSENT, not null: {roster}"
        );
        assert!(
            json["session_actors"].get("s_elsewhere").is_none(),
            "a batch carries the rosters of the sessions it ships and no others"
        );
        assert_eq!(json["events"][0]["actor_id"], "a0123456789abcdef");
        assert_eq!(json["events"][0]["reasoning_excerpt"], "thinking about it");
        assert_eq!(json["events"][0]["reasoning_bytes"], 17);
        // The whole batch still round-trips through the wire types the zod
        // schemas mirror.
        let back: modelstat_wire::IngestBatch = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.events[0].actor_id.as_deref(),
            Some("a0123456789abcdef")
        );
    }

    /// A session nobody stated an actor for ships NO roster. An empty map would
    /// be a claim that the session ran no sub-agents; the honest answer is that
    /// this scan saw none.
    #[tokio::test]
    async fn a_single_agent_session_ships_no_roster_at_all() {
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "x".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "local",
            vec![ev("s1", "2026-07-16T10:00:00.000Z", "hello")],
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &AnsweringRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &Accounts::new(),
        )
        .await;
        let FlushOutcome::Ready(batches) = out else {
            panic!("ready")
        };
        assert!(batches[0].batch.session_actors.is_none());
    }

    #[tokio::test]
    async fn empty_flush_is_ready_with_no_batches() {
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "x".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let mut ev_acc = BTreeMap::new();
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "local",
            Vec::new(),
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &AnsweringRedactor,
            None,
            None,
            &mut acc,
            &mut ev_acc,
            &SessionActors::new(),
            &ScanGeneration::default(),
            &Accounts::new(),
        )
        .await;
        assert!(matches!(out, FlushOutcome::Ready(b) if b.is_empty()));
    }
}
