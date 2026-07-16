//! Batch-assembly primitives — the pure pieces the scan loop (M4 Part 3)
//! composes into an `IngestBatch`. Ports `core/ids.ts::batchId`,
//! `daemon-core/queue::attachSegmentIds`, and
//! `apps/daemon/src/pipeline.ts::prepareCloudRawEvents` (+ its
//! `enrichToolCallRedaction`).
//!
//! Kept here (not in the daemon) because they're pure + testable and operate on
//! pipeline/parsers outputs: the ULID batch id, tool-call → segment attribution,
//! and the cloud-mode raw redaction pass. The scan loop reads state (cursor,
//! mode, the accumulated segment index) and composes these into the actual batch
//! it uploads.

use std::collections::HashMap;

use modelstat_parsers::ToolCallDraft;
use modelstat_redact::{ner_active, ner_redact, redact, NerModel};
use modelstat_wire::{RawEvent, Segment, ToolCallWire};

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

/// Deep-redact each draft's `command_redacted` with the NER pass (layer 2) — the
/// shipped command is the most sensitive field, previously regex-floor only.
/// Deduped per distinct command, best-effort, mutates drafts in place. Port of
/// `enrichToolCallRedaction` (here the redactor is the NER Privacy Filter).
pub fn enrich_tool_call_redaction<N: NerModel>(drafts: &mut [ToolCallDraft], ner: &N) {
    let mut cache: HashMap<String, String> = HashMap::new();
    for draft in drafts.iter_mut() {
        let Some(action) = draft.action.as_mut() else {
            continue;
        };
        let cmd = match action.command_redacted.as_deref().filter(|c| !c.is_empty()) {
            Some(c) => c.to_string(),
            None => continue,
        };
        let deep = if let Some(hit) = cache.get(&cmd) {
            hit.clone()
        } else {
            let d = ner_redact(ner, &cmd).text;
            cache.insert(cmd, d.clone());
            d
        };
        action.command_redacted = Some(deep);
    }
}

/// LAYER-3 deep redaction of the SHIPPED tool commands (§9.5/§21.13) — LOCAL mode
/// only. Runs the LLM backstop ([`crate::passes::redact_backstop`]) over each
/// draft's `command_redacted` on top of the floor (L1) + NER (L2), deduped per
/// distinct command. Fail-safe: a model error / prefilter miss leaves the command
/// UNCHANGED, and the backstop can only ever ADD a redaction of a substring that
/// genuinely appears — never reword, invent, or leak. The caller gates this to
/// `mode == "local"` so the deep pass never crosses the machine boundary.
pub async fn deep_redact_tool_commands<S: crate::Summarizer>(
    drafts: &mut [ToolCallDraft],
    engine: &S,
) {
    let mut cache: HashMap<String, String> = HashMap::new();
    for draft in drafts.iter_mut() {
        let Some(action) = draft.action.as_mut() else {
            continue;
        };
        let cmd = match action.command_redacted.as_deref().filter(|c| !c.is_empty()) {
            Some(c) => c.to_string(),
            None => continue,
        };
        let deep = match cache.get(&cmd) {
            Some(hit) => hit.clone(),
            None => {
                let (redacted, _n) = crate::passes::redact_backstop(engine, &cmd).await;
                cache.insert(cmd, redacted.clone());
                redacted
            }
        };
        action.command_redacted = Some(deep);
    }
}

/// Prepare a Cloud-mode raw batch: run the FULL redaction (regex floor + the
/// on-device NER/PII pass) over every event excerpt AND tool-call command before
/// they leave the machine. FAIL-CLOSED — returns `None` when NER is unavailable,
/// so the caller keeps data local rather than shipping floor-only turns off the
/// box (§9.5/§21.5). Mutates `drafts` (their `command_redacted` gets the NER
/// pass). Port of `prepareCloudRawEvents`.
pub fn prepare_cloud_raw_events<N: NerModel>(
    events: &[RawEvent],
    drafts: &mut [ToolCallDraft],
    ner: &N,
) -> Option<Vec<RawEvent>> {
    if !ner_active(ner) {
        return None; // fail-closed — the caller holds / keeps data local
    }
    let redacted = events
        .iter()
        .map(
            |e| match e.content_excerpt.as_deref().filter(|x| !x.is_empty()) {
                None => e.clone(),
                Some(excerpt) => {
                    let floored = redact(excerpt, None).text;
                    let scrubbed = ner_redact(ner, &floored).text;
                    if scrubbed == excerpt {
                        e.clone()
                    } else {
                        let mut ev = e.clone();
                        ev.content_excerpt = Some(scrubbed);
                        ev
                    }
                }
            },
        )
        .collect();
    if !drafts.is_empty() {
        enrich_tool_call_redaction(drafts, ner);
    }
    Some(redacted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_redact::{NerToken, UnavailableNer};
    use modelstat_wire::{TokenUsage, ToolAction};

    /// A fake "live" NER: tags Katherine Johnson (PER) + Globex Corporation (ORG)
    /// by surface (no offsets → word-boundary path), so `ner_active`'s sentinel
    /// scrubs and any test text carrying those names gets redacted.
    struct FakeNer;
    impl NerModel for FakeNer {
        fn classify(&self, _text: &str) -> Option<Vec<NerToken>> {
            let tok = |ent: &str, word: &str| NerToken {
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
        }
    }

    fn ev(source_event_id: &str, excerpt: Option<&str>) -> RawEvent {
        RawEvent {
            source_event_id: source_event_id.into(),
            ts: "2026-07-16T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s1".into(),
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: None,
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: excerpt.map(Into::into),
            references: None,
            source_file: None,
            source_byte_offset: None,
            pricing_mode: None,
        }
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
        map.insert("e_earlier".to_string(), "seg_from_earlier_batch".to_string());
        let wire = attach_segment_ids_by_map(&calls, &map);
        assert_eq!(wire[0].segment_id.as_deref(), Some("seg_from_earlier_batch"));
    }

    #[test]
    fn cloud_raw_events_fail_closed_when_ner_down() {
        let events = vec![ev("e1", Some("hello"))];
        let mut drafts = vec![draft("c1", "e1", Some("ssh prod"))];
        assert!(prepare_cloud_raw_events(&events, &mut drafts, &UnavailableNer).is_none());
        // Fail-closed must NOT have mutated the drafts.
        assert_eq!(
            drafts[0].action.as_ref().unwrap().command_redacted.as_deref(),
            Some("ssh prod")
        );
    }

    #[test]
    fn cloud_raw_events_ner_scrub_excerpts_and_commands() {
        let events = vec![
            ev("e1", Some("Escalate to Katherine Johnson now")),
            ev("e2", Some("no entities here")),
            ev("e3", None),
        ];
        let mut drafts = vec![draft("c1", "e1", Some("mail Katherine Johnson"))];
        let out = prepare_cloud_raw_events(&events, &mut drafts, &FakeNer).expect("ner active");
        assert_eq!(
            out[0].content_excerpt.as_deref(),
            Some("Escalate to [REDACTED:PER] now")
        );
        // Unchanged excerpt keeps the original event untouched.
        assert_eq!(out[1].content_excerpt.as_deref(), Some("no entities here"));
        assert_eq!(out[2].content_excerpt, None);
        // The shipped command got the NER pass.
        assert_eq!(
            drafts[0].action.as_ref().unwrap().command_redacted.as_deref(),
            Some("mail [REDACTED:PER]")
        );
    }

    #[tokio::test]
    async fn deep_redact_replaces_named_secrets_in_local_commands() {
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
        deep_redact_tool_commands(&mut drafts, &FakeEngine).await;
        let cmd = drafts[0]
            .action
            .as_ref()
            .unwrap()
            .command_redacted
            .as_deref()
            .unwrap();
        assert!(cmd.contains("[REDACTED:llm]"), "L3 must redact the named secret: {cmd}");
        assert!(!cmd.contains("sk_live_abcdef123456"));
    }
}
