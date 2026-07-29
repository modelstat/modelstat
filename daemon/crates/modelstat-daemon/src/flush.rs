//! Batch flush — the composition at the heart of `apps/daemon/src/scan.ts`'s
//! `flushBatch`, lifted out of the scan I/O so it's pure + testable with a fake
//! engine. Given a buffer of git-corrected events + tool-call drafts, it produces
//! the `IngestBatch`(es) to ship — or holds.
//!
//! Two paths, keyed on the summariser mode:
//!   - **cloud**: redact the raw turns on-device ([`prepare_cloud_raw_events`],
//!     FAIL-CLOSED) and emit ONE raw batch per session (no local segments — the
//!     server summarises). NER down ⇒ [`FlushOutcome::Held`] (no-degrade: we hold
//!     + retry rather than ship floor-only-redacted turns, §21.5).
//!   - **local / self-hosted**: [`build_for_one_session`] per session (any
//!     `Held` ⇒ hold the whole flush, never a partial batch), then titles +
//!     metadata + segment attribution → one `IngestBatch`.
//!
//! The scan loop owns the I/O: it uploads each [`PreparedBatch`] and advances the
//! buffered files' cursors only on a confirmed commit (never-drop).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use modelstat_parsers::{GitEnrichment, ToolCallDraft};
use modelstat_pipeline::{
    attach_segment_ids_by_map, batch_id, build_for_one_session, build_session_metadata,
    build_session_titles, deep_redact_tool_commands, enrich_tool_call_redaction,
    prepare_cloud_raw_events, BuildOutcome, Embedder, LinkExtractor, ResilientSummarizer,
    Summarizer,
};
use modelstat_redact::NerModel;
use modelstat_wire::{IngestBatch, RawEvent, Segment, TokenUsage};

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
pub async fn build_flush_batches<S, E, N>(
    device_id: &str,
    daemon_version: &str,
    mode: &str,
    events: Vec<RawEvent>,
    mut drafts: Vec<ToolCallDraft>,
    resilient: &ResilientSummarizer<S>,
    embedder: &E,
    ner: &N,
    // Moved into the metadata pass (its only use), so no reborrow lifetime dance.
    // `+ Send`: this runs inside the daemon's tokio-spawned single-flight scan,
    // whose future must be `Send` (a `dyn Trait` erases the concrete Send-ness).
    git: Option<&mut (dyn GitEnrichment + Send)>,
    extract_links: Option<&LinkExtractor<'_>>,
    run_segments_by_session: &mut BTreeMap<String, Vec<Segment>>,
) -> FlushOutcome
where
    S: Summarizer,
    E: Embedder,
    N: NerModel,
{
    if events.is_empty() && drafts.is_empty() {
        return FlushOutcome::Ready(Vec::new());
    }
    let events: Vec<RawEvent> = events.into_iter().map(with_non_null_tokens).collect();

    // ── Cloud: redact on-device, ship raw, summarise server-side ──
    if mode == "cloud" {
        let Some(cloud_events) = prepare_cloud_raw_events(&events, &mut drafts, ner) else {
            // FAIL-CLOSED + no-degrade: the on-device NER redactor is unavailable,
            // so turns can't be scrubbed before leaving the box. HOLD + retry (the
            // TS shipped local extractive; the rewrite never degrades).
            modelstat_log::log_warn!(
                "cloud NER/PII redactor unavailable — holding this flush \
                 (no raw egress, no degrade); retrying once the model is ready"
            );
            return FlushOutcome::Held;
        };
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
        let out = by_session
            .into_iter()
            .map(|(_sid, (evs, calls))| PreparedBatch {
                batch: IngestBatch {
                    batch_id: batch_id(),
                    device_id: device_id.into(),
                    daemon_version: daemon_version.into(),
                    events: evs,
                    segments: Vec::new(),
                    tool_calls: calls,
                    session_installs: None,
                    session_titles: None,
                    session_metadata: None,
                    summarizer_mode: None,
                },
                raw: true,
                segment_count: 0,
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
        match build_for_one_session(sess_events, resilient, embedder, ner).await {
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

    // Deep-redact the SHIPPED tool commands before attribution: L2 (NER, always
    // on-device) + L3 (LLM backstop, LOCAL mode only — §21.13, never crosses the
    // machine boundary). Both fail-safe: a down NER/engine leaves the L1-floored
    // command unchanged. The cloud path already ran L2 inside prepare_cloud_raw_events.
    enrich_tool_call_redaction(&mut drafts, ner);
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
    let batch = IngestBatch {
        batch_id: batch_id(),
        device_id: device_id.into(),
        daemon_version: daemon_version.into(),
        events,
        segments,
        tool_calls,
        session_installs: None,
        session_titles: if session_titles.is_empty() {
            None
        } else {
            Some(session_titles)
        },
        session_metadata: session_metadata_value,
        summarizer_mode: None,
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
    use modelstat_pipeline::NoEmbedder;
    use modelstat_redact::UnavailableNer;
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

    fn ev(session: &str, ts: &str, excerpt: &str) -> RawEvent {
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
            content_excerpt: Some(excerpt.into()),
            references: None,
            source_file: None,
            source_byte_offset: None,
            pricing_mode: None,
        }
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
            &UnavailableNer,
            None,
            None,
            &mut acc,
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
        let events = vec![ev("s1", "2026-07-16T10:00:00.000Z", "hello")];
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "local",
            events,
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            None,
            None,
            &mut acc,
        )
        .await;
        assert!(matches!(out, FlushOutcome::Held));
    }

    #[tokio::test]
    async fn cloud_mode_holds_when_ner_is_down() {
        // Fail-closed: cloud + UnavailableNer → Held (no raw egress, no degrade).
        let resilient = ResilientSummarizer::with_cooldown(
            Fake {
                reply: "x".into(),
                failing: false,
            },
            Duration::ZERO,
        );
        let mut acc = BTreeMap::new();
        let events = vec![ev("s1", "2026-07-16T10:00:00.000Z", "secret stuff")];
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "cloud",
            events,
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            None,
            None,
            &mut acc,
        )
        .await;
        assert!(matches!(out, FlushOutcome::Held));
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
        let out = build_flush_batches(
            "dev1",
            "9.9.9",
            "local",
            Vec::new(),
            Vec::new(),
            &resilient,
            &NoEmbedder,
            &UnavailableNer,
            None,
            None,
            &mut acc,
        )
        .await;
        assert!(matches!(out, FlushOutcome::Ready(b) if b.is_empty()));
    }
}
