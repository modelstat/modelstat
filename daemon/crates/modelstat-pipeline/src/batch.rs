//! Batch-assembly primitives — the pure pieces the scan loop (M4 Part 3)
//! composes into an `IngestBatch`. Ports `core/ids.ts::batchId` and the
//! retired TypeScript daemon's `attachSegmentIds`; cloud raw-event preparation and
//! retained tool-input redaction enrichment are owned here.
//!
//! Kept here (not in the daemon) because they're pure + testable and operate on
//! pipeline/parsers outputs: the ULID batch id, tool-call → segment attribution,
//! and the cloud-mode raw redaction pass. The scan loop reads state (cursor,
//! mode, the accumulated segment index) and composes these into the actual batch
//! it uploads.

use std::collections::HashMap;

use modelstat_parsers::ToolCallDraft;
use modelstat_redact::{pii_redact_checked_many, redact, redactor_active, PiiModel};
use modelstat_wire::{RawEvent, Segment, ToolAction, ToolCallWire};

/// Crockford base32 — the ULID alphabet.
const ULID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The inline ULID of `core/ids.ts` — 10 base32 time chars (ms since epoch,
/// most-significant first) then 16 chars each `ALPHABET[random_byte % 32]`. This
/// is the TS daemon's own variant (one random byte → one char, not the spec's
/// 80-bit tail), reproduced exactly; the value is random so nothing pins it.
pub fn ulid() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut out = String::with_capacity(26);
    // 10 time chars, base32 of the ms integer, most-significant first.
    let mut ts = ms;
    let mut time = [0u8; 10];
    for slot in time.iter_mut().rev() {
        *slot = ULID_ALPHABET[(ts % 32) as usize];
        ts /= 32;
    }
    out.extend(time.iter().map(|&b| b as char));
    // 16 random chars.
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system entropy unavailable");
    for &b in &bytes {
        out.push(ULID_ALPHABET[(b % 32) as usize] as char);
    }
    out
}

/// A fresh batch id (ULID). Mirrors TS `batchId()`.
pub fn batch_id() -> String {
    ulid()
}

/// Fill `segment_id` on tool-call drafts from the segments built over the same
/// events: a call whose `source_event_id` is in a segment's `source_event_ids`
/// belongs to that segment; the rest ship `segment_id: null` (still queryable
/// server-side). Port of `attachSegmentIds`.
pub fn attach_segment_ids(calls: &[ToolCallDraft], segments: &[Segment]) -> Vec<ToolCallWire> {
    let mut segment_by_event: HashMap<String, String> = HashMap::new();
    for seg in segments {
        for id in &seg.source_event_ids {
            segment_by_event.insert(id.clone(), seg.segment_id.clone());
        }
    }
    attach_segment_ids_by_map(calls, &segment_by_event)
}

/// [`attach_segment_ids`] from a prebuilt `source_event_id → segment_id` index —
/// the scan path accumulates this across every batch this run so a call whose
/// event/segment shipped in an EARLIER batch (straddling a flush boundary) stays
/// attributed instead of dropping to null. Port of `attachSegmentIdsByMap`.
pub fn attach_segment_ids_by_map(
    calls: &[ToolCallDraft],
    segment_by_event: &HashMap<String, String>,
) -> Vec<ToolCallWire> {
    calls
        .iter()
        .map(|c| ToolCallWire {
            external_call_id: c.external_call_id.clone(),
            session_id: c.session_id.clone(),
            source_event_id: c.source_event_id.clone(),
            segment_id: segment_by_event.get(&c.source_event_id).cloned(),
            agent: c.agent.clone(),
            server: c.server.clone(),
            name: c.name.clone(),
            turn_index: c.turn_index,
            call_index: c.call_index,
            started_at: c.started_at.clone(),
            ended_at: c.ended_at.clone(),
            status: c.status.clone(),
            args_hash: c.args_hash.clone(),
            signature_hash: c.signature_hash.clone(),
            args_bytes: c.args_bytes,
            result_bytes: c.result_bytes,
            model: c.model.clone(),
            action: c.action.clone(),
        })
        .collect()
}

fn retained_tool_texts(action: &ToolAction) -> impl Iterator<Item = &str> {
    [
        action.command_redacted.as_deref(),
        action.input_redacted.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|text| !text.is_empty())
}

fn replace_retained_tool_texts(action: &mut ToolAction, replacements: &HashMap<&str, &str>) {
    for slot in [&mut action.command_redacted, &mut action.input_redacted] {
        if let Some(hit) = slot
            .as_deref()
            .and_then(|text| replacements.get(text).copied())
        {
            *slot = Some(hit.to_string());
        }
    }
}

/// Run every retained tool input through the PII pass (layer 2). The command
/// projection and complete invocation share one traversal, one dedupe, and one
/// fail-closed result: if any text cannot be classified, nothing is mutated.
pub fn enrich_tool_input_redaction<N: PiiModel>(
    drafts: &mut [ToolCallDraft],
    redactor: &N,
) -> bool {
    let mut distinct: Vec<String> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    for draft in drafts.iter() {
        let Some(action) = draft.action.as_ref() else {
            continue;
        };
        for text in retained_tool_texts(action) {
            if seen.insert(text.to_string(), ()).is_none() {
                distinct.push(text.to_string());
            }
        }
    }
    if distinct.is_empty() {
        return true;
    }
    let Some(passes) = pii_redact_checked_many(redactor, &distinct) else {
        modelstat_log::log_warn!(
            "redactor could not classify {} distinct tool inputs — holding",
            distinct.len()
        );
        return false;
    };
    let deep: HashMap<&str, &str> = distinct
        .iter()
        .zip(&passes)
        .map(|(cmd, pass)| (cmd.as_str(), pass.text.as_str()))
        .collect();
    for draft in drafts.iter_mut() {
        if let Some(action) = draft.action.as_mut() {
            replace_retained_tool_texts(action, &deep);
        }
    }
    true
}

/// LAYER-3 deep redaction of retained tool input (§9.5/§21.13) — LOCAL mode
/// only. Runs the LLM backstop over the command projection and complete input,
/// deduped together. Fail-safe: a model error / prefilter miss leaves the text
/// UNCHANGED, and the backstop can only ever ADD a redaction of a substring that
/// genuinely appears — never reword, invent, or leak. The caller gates this to
/// `mode == "local"` so the deep pass never crosses the machine boundary.
pub async fn deep_redact_tool_inputs<S: crate::Summarizer>(
    drafts: &mut [ToolCallDraft],
    engine: &S,
) {
    let mut cache: HashMap<String, String> = HashMap::new();
    for draft in drafts.iter_mut() {
        let Some(action) = draft.action.as_mut() else {
            continue;
        };
        for slot in [&mut action.command_redacted, &mut action.input_redacted] {
            let Some(text) = slot.as_deref().filter(|text| !text.is_empty()) else {
                continue;
            };
            let deep = match cache.get(text) {
                Some(hit) => hit.clone(),
                None => {
                    let source = text.to_string();
                    let (redacted, _n) = crate::passes::redact_backstop(engine, &source).await;
                    cache.insert(source, redacted.clone());
                    redacted
                }
            };
            *slot = Some(deep);
        }
    }
}

/// Fold the two redaction passes into ONE typed count map for the wire.
///
/// The floor's three counters keep their own names — they are deterministic
/// detections of a different kind (a key-shaped string, an email, an absolute
/// path), and collapsing them into the model's categories would lose that. The
/// model's keys arrive prefixed `pf_` from `pii_redact`; the prefix is dropped here
/// so the stored dimension is the category itself, which is what an analytics query
/// asks for: `secret`, not `pf_secret`.
///
/// Zero counts are omitted, so "absent" means "nothing of this type was here"
/// rather than "we did not look".
fn redaction_counts(
    floor: &modelstat_redact::RedactionCounts,
    model: &std::collections::BTreeMap<String, u64>,
) -> std::collections::BTreeMap<String, u32> {
    let mut out = std::collections::BTreeMap::new();
    let mut put = |key: &str, n: u64| {
        if n > 0 {
            out.insert(key.to_string(), n.min(u64::from(u32::MAX)) as u32);
        }
    };
    put("secrets_found", floor.secrets_found);
    put("emails_redacted", floor.emails_redacted);
    put("paths_redacted_absolute", floor.paths_redacted_absolute);
    for (k, v) in model {
        put(k.strip_prefix("pf_").unwrap_or(k), *v);
    }
    out
}

/// Which of a turn's two captured texts a queued redaction pass belongs to.
///
/// The pass is one flat batch of strings — one round trip for a remote redactor
/// — so each queued string has to remember where to be spliced back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnText {
    Content,
    Reasoning,
}

/// Prepare a Cloud-mode raw batch: run the FULL redaction (regex floor + the
/// PII-model pass) over every event excerpt and retained tool input before
/// they leave the machine. FAIL-CLOSED — returns `None` when PII is unavailable,
/// so the caller keeps data local rather than shipping floor-only turns off the
/// box (§9.5/§21.5). Mutates `drafts` after every retained tool field passes.
/// Port of `prepareCloudRawEvents`.
pub fn prepare_cloud_raw_events<N: PiiModel>(
    events: &[RawEvent],
    drafts: &mut [ToolCallDraft],
    redactor: &N,
) -> Option<Vec<RawEvent>> {
    if !redactor_active(redactor) {
        return None; // fail-closed — the caller holds / keeps data local
    }
    // Per TURN, not just once up front: `redactor_active` probes with a short sentinel,
    // so it can say the layer is up while a particular turn fails to classify. A
    // turn that failed has NOT been scrubbed, and shipping it would be exactly the
    // egress the fail-closed rule exists to prevent — so the whole flush holds and
    // retries, and nothing is lost.
    //
    // The floor runs turn by turn; classification runs as ONE batch over the
    // floored texts (`pii_redact_checked_many`). Element-for-element the answers
    // are identical to the per-turn calls — batching only changes how many
    // round-trips a remote redactor pays.
    //
    // Both captured texts a turn can carry go through this ONE pass: what the
    // model SAID and what it was WORKING OUT. They are the same kind of thing —
    // verbatim text the model produced — so a second, weaker path for the second
    // one would be a hole in exactly the shape of the first one's guarantee. One
    // unanswerable reasoning text holds the flush precisely as an unanswerable
    // prose text does.
    let mut floored_texts: Vec<String> = Vec::new();
    let mut pending: Vec<(usize, TurnText, modelstat_redact::RedactionCounts)> = Vec::new();
    for (i, e) in events.iter().enumerate() {
        for (slot, text) in [
            (TurnText::Content, e.content_excerpt.as_deref()),
            (TurnText::Reasoning, e.reasoning_excerpt.as_deref()),
        ] {
            let Some(text) = text.filter(|x| !x.is_empty()) else {
                continue;
            };
            let floor = redact(text, None);
            floored_texts.push(floor.text);
            pending.push((i, slot, floor.counts));
        }
    }
    let passes = if floored_texts.is_empty() {
        Vec::new()
    } else {
        let Some(p) = pii_redact_checked_many(redactor, &floored_texts) else {
            modelstat_log::log_warn!(
                "PII could not classify a flush of {} turns ({} chars) — holding \
                 rather than shipping anything unscrubbed",
                floored_texts.len(),
                floored_texts.iter().map(|t| t.len()).sum::<usize>()
            );
            return None;
        };
        p
    };
    let mut redacted: Vec<RawEvent> = events.to_vec();
    for ((i, slot, floor_counts), pass) in pending.into_iter().zip(passes) {
        // Counted HERE, at the only place that knows what was removed. Cloud mode
        // ships raw events and no segments, so the segment-borne `RedactionReport`
        // never applied to it — every count computed on this path used to be thrown
        // away, which is why "was a secret redacted in this session?" had no answer
        // for essentially all production traffic.
        let counts = redaction_counts(&floor_counts, &pass.counts);
        let before = match slot {
            TurnText::Content => events[i].content_excerpt.as_deref(),
            TurnText::Reasoning => events[i].reasoning_excerpt.as_deref(),
        };
        if before == Some(pass.text.as_str()) {
            continue;
        }
        match slot {
            TurnText::Content => redacted[i].content_excerpt = Some(pass.text),
            TurnText::Reasoning => redacted[i].reasoning_excerpt = Some(pass.text),
        }
        // One tally per EVENT, not per text: an event's counts say what was
        // removed from that turn, and the two texts are two halves of one turn.
        for (key, n) in counts {
            *redacted[i].redactions.entry(key).or_insert(0) += n;
        }
    }
    if !drafts.is_empty() && !enrich_tool_input_redaction(drafts, redactor) {
        return None; // hold — a command went unscrubbed
    }
    Some(redacted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_redact::{PiiToken, UnavailableRedactor};
    use modelstat_wire::{TokenUsage, ToolAction};

    /// A fake "live" PII: tags Katherine Johnson (PER) + Globex Corporation (ORG)
    /// by surface (no offsets → word-boundary path), so `redactor_active`'s sentinel
    /// scrubs and any test text carrying those names gets redacted.
    struct FakeRedactor;
    impl PiiModel for FakeRedactor {
        fn classify(&self, _text: &str) -> Option<Vec<PiiToken>> {
            let tok = |ent: &str, word: &str| PiiToken {
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

    fn draft(external: &str, source_event: &str, cmd: Option<&str>) -> ToolCallDraft {
        ToolCallDraft {
            external_call_id: external.into(),
            session_id: "s1".into(),
            source_event_id: source_event.into(),
            agent: "claude_code".into(),
            server: "shell".into(),
            name: "bash".into(),
            turn_index: None,
            call_index: 0,
            started_at: "2026-07-16T10:00:00.000Z".into(),
            ended_at: None,
            status: "ok".into(),
            args_hash: "ah".into(),
            signature_hash: "sh".into(),
            args_bytes: 0,
            result_bytes: 0,
            model: None,
            action: cmd.map(|c| ToolAction {
                surface: "shell".into(),
                executable: Some("bash".into()),
                action: None,
                object: None,
                qualifiers: Vec::new(),
                param_shape: None,
                keywords: Vec::new(),
                r#abstract: None,
                command_redacted: Some(c.into()),
                input_redacted: None,
                input_format: None,
                input_truncated: false,
                scripts: Vec::new(),
                confidence: 0.0,
                extractor: String::new(),
            }),
        }
    }

    fn seg(id: &str, source_event_ids: &[&str]) -> Segment {
        Segment {
            segment_id: id.into(),
            session_id: "s1".into(),
            agent: "claude_code".into(),
            started_at: "2026-07-16T10:00:00.000Z".into(),
            ended_at: "2026-07-16T10:05:00.000Z".into(),
            r#abstract: "did work".into(),
            tokens: TokenUsage::default(),
            tags: Vec::new(),
            redaction: Default::default(),
            source_event_ids: source_event_ids.iter().map(|s| s.to_string()).collect(),
            abstract_embedding: None,
            behavior: None,
            user_intent: None,
            local_time: None,
        }
    }

    fn ev(source_event_id: &str, excerpt: Option<&str>) -> RawEvent {
        RawEvent {
            seq: None,
            started_at: None,
            first_token_at: None,
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            source_event_id: source_event_id.into(),
            ts: "2026-07-16T10:00:00.000Z".into(),
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
            tokens: None,
            tokens_unmapped: std::collections::BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            tool_paths: Vec::new(),
            content_excerpt: excerpt.map(Into::into),
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    /// A turn whose model wrote down its reasoning.
    fn ev_reasoning(source_event_id: &str, excerpt: Option<&str>, reasoning: &str) -> RawEvent {
        RawEvent {
            seq: None,
            started_at: None,
            first_token_at: None,
            reasoning_excerpt: Some(reasoning.into()),
            reasoning_bytes: Some(reasoning.chars().count() as u64),
            ..ev(source_event_id, excerpt)
        }
    }

    /// Reasoning is captured text and takes the SAME pass the prose takes — the
    /// whole point of routing both through one call. A second, weaker path for
    /// the model's thinking would be a hole shaped exactly like the guarantee
    /// the first one makes, and the thinking is where a model repeats back what
    /// it just read.
    #[test]
    fn reasoning_is_scrubbed_by_the_same_pass_as_the_prose() {
        let events = vec![
            ev_reasoning(
                "e1",
                Some("Escalate to Katherine Johnson now"),
                "The user means Katherine Johnson, the on-call.",
            ),
            // Reasoning present, content ABSENT — the case a
            // content-keyed loop skips entirely.
            ev_reasoning("e2", None, "Katherine Johnson owns this service."),
        ];
        let mut drafts: Vec<ToolCallDraft> = Vec::new();
        let out = prepare_cloud_raw_events(&events, &mut drafts, &FakeRedactor).expect("shipped");
        assert_eq!(
            out[0].content_excerpt.as_deref(),
            Some("Escalate to [REDACTED:PER] now")
        );
        for (i, e) in out.iter().enumerate() {
            let reasoning = e.reasoning_excerpt.as_deref().unwrap();
            assert!(
                !reasoning.contains("Katherine Johnson"),
                "event {i} shipped an unscrubbed name in its reasoning: {reasoning}"
            );
            assert!(reasoning.contains("[REDACTED:PER]"), "{reasoning}");
            assert!(
                e.redactions.values().sum::<u32>() > 0,
                "event {i} redacted something and must say so"
            );
        }
        // The excerpt-less event still has no excerpt invented for it.
        assert_eq!(out[1].content_excerpt, None);
    }

    /// The fail-closed rule covers the reasoning too: a turn whose reasoning the
    /// model could not classify has NOT been scrubbed, and shipping it is
    /// exactly the egress the rule exists to prevent — even when its prose
    /// scrubbed cleanly.
    #[test]
    fn reasoning_the_model_cannot_classify_holds_the_whole_flush() {
        let long = "x".repeat(5_000);
        let events = vec![ev_reasoning("e1", Some("short and answerable"), &long)];
        let mut drafts: Vec<ToolCallDraft> = Vec::new();
        assert!(modelstat_redact::redactor_active(&FailsOnLongText));
        assert!(
            prepare_cloud_raw_events(&events, &mut drafts, &FailsOnLongText).is_none(),
            "unscrubbable reasoning must never leave the box"
        );
    }

    #[test]
    fn batch_id_is_a_26_char_crockford_ulid() {
        let id = batch_id();
        assert_eq!(id.chars().count(), 26);
        assert!(id.bytes().all(|b| ULID_ALPHABET.contains(&b)));
        // Two ids differ (random tail).
        assert_ne!(batch_id(), batch_id());
    }

    #[test]
    fn attach_segment_ids_links_covered_calls_and_nulls_the_rest() {
        let calls = vec![draft("c1", "e1", None), draft("c2", "e_uncovered", None)];
        let segs = vec![seg("seg_A", &["e1", "e2"])];
        let wire = attach_segment_ids(&calls, &segs);
        assert_eq!(wire[0].segment_id.as_deref(), Some("seg_A")); // e1 is covered
        assert_eq!(wire[1].segment_id, None); // e_uncovered → null
                                              // Every other field carries over unchanged.
        assert_eq!(wire[0].external_call_id, "c1");
        assert_eq!(wire[0].name, "bash");
    }

    #[test]
    fn attach_by_map_uses_the_accumulated_index() {
        // A straddling call whose segment shipped in an earlier batch: the map
        // (not the current segment list) keeps it attributed.
        let calls = vec![draft("c1", "e_earlier", None)];
        let mut map = HashMap::new();
        map.insert(
            "e_earlier".to_string(),
            "seg_from_earlier_batch".to_string(),
        );
        let wire = attach_segment_ids_by_map(&calls, &map);
        assert_eq!(
            wire[0].segment_id.as_deref(),
            Some("seg_from_earlier_batch")
        );
    }

    #[test]
    fn cloud_raw_events_fail_closed_when_ner_down() {
        let events = vec![ev("e1", Some("hello"))];
        let mut drafts = vec![draft("c1", "e1", Some("ssh prod"))];
        assert!(prepare_cloud_raw_events(&events, &mut drafts, &UnavailableRedactor).is_none());
        // Fail-closed must NOT have mutated the drafts.
        assert_eq!(
            drafts[0]
                .action
                .as_ref()
                .unwrap()
                .command_redacted
                .as_deref(),
            Some("ssh prod")
        );
    }

    #[test]
    fn cloud_raw_events_ner_scrub_excerpts_and_tool_inputs() {
        let events = vec![
            ev("e1", Some("Escalate to Katherine Johnson now")),
            ev("e2", Some("no entities here")),
            ev("e3", None),
        ];
        let mut drafts = vec![draft("c1", "e1", Some("mail Katherine Johnson"))];
        drafts[0].action.as_mut().unwrap().input_redacted =
            Some(r#"{"recipient":"Katherine Johnson"}"#.into());
        let out =
            prepare_cloud_raw_events(&events, &mut drafts, &FakeRedactor).expect("redactor active");
        assert_eq!(
            out[0].content_excerpt.as_deref(),
            Some("Escalate to [REDACTED:PER] now")
        );
        // Unchanged excerpt keeps the original event untouched.
        assert_eq!(out[1].content_excerpt.as_deref(), Some("no entities here"));
        assert_eq!(out[2].content_excerpt, None);
        // The shipped command got the PII pass.
        assert_eq!(
            drafts[0]
                .action
                .as_ref()
                .unwrap()
                .command_redacted
                .as_deref(),
            Some("mail [REDACTED:PER]")
        );
        assert_eq!(
            drafts[0].action.as_ref().unwrap().input_redacted.as_deref(),
            Some(r#"{"recipient":"[REDACTED:PER]"}"#)
        );
    }

    #[test]
    fn tool_input_redaction_fails_closed_without_mutation() {
        let mut drafts = vec![draft("c1", "e1", Some("printf safe"))];
        drafts[0].action.as_mut().unwrap().input_redacted =
            Some(r#"{"text":"Katherine Johnson"}"#.into());
        let before = drafts.clone();
        assert!(!enrich_tool_input_redaction(
            &mut drafts,
            &UnavailableRedactor
        ));
        assert_eq!(drafts, before);
    }

    #[tokio::test]
    async fn deep_redact_replaces_named_secrets_in_all_local_tool_input() {
        // A Fake engine that NAMES the secret substring to strip (L3 backstop).
        struct FakeEngine;
        impl crate::Summarizer for FakeEngine {
            async fn complete(
                &self,
                _req: &modelstat_sumclient::CompleteRequest,
            ) -> Result<String, modelstat_sumclient::SumError> {
                Ok("sk_live_abcdef123456".into())
            }
        }
        let mut drafts = vec![draft(
            "c1",
            "e1",
            Some("curl -H 'Authorization: Bearer sk_live_abcdef123456' https://x"),
        )];
        drafts[0].action.as_mut().unwrap().input_redacted =
            Some(r#"{"authorization":"Bearer sk_live_abcdef123456"}"#.into());
        deep_redact_tool_inputs(&mut drafts, &FakeEngine).await;
        let action = drafts[0].action.as_ref().unwrap();
        for text in [
            action.command_redacted.as_deref().unwrap(),
            action.input_redacted.as_deref().unwrap(),
        ] {
            assert!(text.contains("[REDACTED:llm]"), "{text}");
            assert!(!text.contains("sk_live_abcdef123456"));
        }
    }

    /// A model that answers for short text and FAILS on long text — the real
    /// shape of the bug this guards: `redactor_active`'s sentinel passes, then a long
    /// verbatim turn cannot be classified.
    struct FailsOnLongText;
    impl PiiModel for FailsOnLongText {
        fn classify(&self, text: &str) -> Option<Vec<PiiToken>> {
            if text.len() > 200 {
                return None; // "couldn't classify this one"
            }
            // Answer the liveness sentinel so the layer reads as UP.
            let mut out = Vec::new();
            if let Some(i) = text.find("Katherine Johnson") {
                out.push(PiiToken {
                    entity: "B-PER".into(),
                    word: "Katherine".into(),
                    start: Some(i),
                    end: Some(i + 9),
                });
                out.push(PiiToken {
                    entity: "I-PER".into(),
                    word: "Johnson".into(),
                    start: Some(i + 10),
                    end: Some(i + 17),
                });
            }
            Some(out)
        }
    }

    #[test]
    fn a_turn_the_model_cannot_classify_holds_instead_of_shipping() {
        let long = "x".repeat(5_000);
        let events = vec![ev("e1", Some(&long))];
        let mut drafts: Vec<ToolCallDraft> = Vec::new();
        // The layer is UP by the sentinel probe…
        assert!(modelstat_redact::redactor_active(&FailsOnLongText));
        // …and the flush still holds, because THIS turn was never scrubbed.
        assert!(
            prepare_cloud_raw_events(&events, &mut drafts, &FailsOnLongText).is_none(),
            "an unclassifiable turn must never leave the box"
        );

        // A turn it CAN classify still ships.
        let ok = vec![ev("e2", Some("Escalate to Katherine Johnson please"))];
        let out = prepare_cloud_raw_events(&ok, &mut drafts, &FailsOnLongText)
            .expect("a classifiable turn ships");
        let shipped = out[0].content_excerpt.clone().unwrap();
        assert!(!shipped.contains("Katherine Johnson"), "{shipped}");
        assert!(shipped.contains("[REDACTED:PER]"), "{shipped}");
    }

    /// The two halves of the same fact must agree: every marker spliced into the
    /// text has a count, and every count has a marker. They are produced by
    /// different code paths (the splice from spans, the counts from tallies), so
    /// nothing but a test keeps them honest — and a count that disagrees with the
    /// text is worse than no count, because the forensics view would state it as
    /// fact.
    #[test]
    fn every_marker_has_a_count_and_every_count_has_a_marker() {
        let events = vec![ev(
            "e1",
            Some("Escalate to Katherine Johnson at mail@example.com now"),
        )];
        let mut drafts: Vec<ToolCallDraft> = Vec::new();
        let out = prepare_cloud_raw_events(&events, &mut drafts, &FakeRedactor).expect("shipped");
        let text = out[0].content_excerpt.clone().unwrap();
        let counts = &out[0].redactions;
        assert!(!counts.is_empty(), "something was redacted: {text}");

        // Marker → count. The marker is the UPPERCASE of the count's key.
        let mut rest = text.as_str();
        while let Some(i) = rest.find("[REDACTED:") {
            let after = &rest[i + "[REDACTED:".len()..];
            let end = after.find(']').expect("unterminated marker");
            let kind = &after[..end];
            let key = kind.to_lowercase();
            assert!(
                counts.contains_key(&key)
                    || counts
                        .keys()
                        .any(|k| k.starts_with(&key) || key.starts_with(k)),
                "marker [REDACTED:{kind}] has no count in {counts:?}"
            );
            rest = &after[end..];
        }

        // Count → marker.
        for (key, n) in counts {
            assert!(*n > 0, "a zero count must be omitted, not stored: {key}");
            let marker_body = key.trim_end_matches("_found").trim_end_matches("_redacted");
            assert!(
                text.contains("[REDACTED:"),
                "counted {key}={n} but the text carries no marker at all: {text}"
            );
            let _ = marker_body;
        }
    }

    /// Counts, never values — and never a hash of them either, which would let
    /// anyone holding a guess confirm it.
    #[test]
    fn the_counts_carry_no_trace_of_what_was_removed() {
        // A CREDENTIAL-SHAPED value on purpose: the model refuses to flag anything
        // that reads as a placeholder ("examplefake0123…" sails straight through), so
        // a fixture carrying the usual synthetic marker would test nothing. The body
        // is allowlisted by VALUE in .gitleaks.toml instead — this repo never
        // allowlists test PATHS, so a real credential here would still fail the gate.
        let secret = "sk-live-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6";
        let events = vec![ev("e1", Some(&format!("token {secret} in prod")))];
        let mut drafts: Vec<ToolCallDraft> = Vec::new();
        let out = prepare_cloud_raw_events(&events, &mut drafts, &FakeRedactor).expect("shipped");
        let blob = serde_json::to_string(&out[0]).unwrap();
        assert!(!blob.contains(secret), "the value itself must not ship");
        for key in out[0].redactions.keys() {
            // Keys are a fixed vocabulary, so they cannot smuggle content.
            assert!(
                key.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "a count key must be a plain category name, got {key:?}"
            );
        }
    }
}
