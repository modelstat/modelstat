//! Golden parity — §4.7 (wire) + D12 byte clamp. Proves the Rust structs ACCEPT
//! the TS-produced wire (the first D16 direction) and round-trip it losslessly.
//! The reverse direction (TS accepts Rust wire) is proven by
//! `packages/core/src/wire-parity.test.ts` against the `wire/rust-emitted/`
//! fixtures written by `examples/emit_fixtures.rs`.

use modelstat_wire::clamp_utf8_bytes;
use modelstat_wire::{HeartbeatPayload, IngestBatch, RawEvent, Segment, ToolCallWire};
use serde::de::DeserializeOwned;
use serde::Serialize;

fn read(name: &str) -> String {
    let path = format!("{}/tests/golden/wire/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Deserialize a fixture, then prove serialize→deserialize is a stable identity
/// on the Rust struct (no field silently lost or reshaped).
fn roundtrip<T: DeserializeOwned + Serialize + PartialEq + std::fmt::Debug>(name: &str) -> T {
    let text = read(name);
    let value: T =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("deserialize {name}: {e}"));
    let reser = serde_json::to_string(&value).unwrap();
    let back: T =
        serde_json::from_str(&reser).unwrap_or_else(|e| panic!("re-deserialize {name}: {e}"));
    assert_eq!(value, back, "round-trip changed {name}");
    value
}

#[test]
fn raw_event_full_accepts_and_roundtrips() {
    let ev: RawEvent = roundtrip("raw_event_full.json");
    assert_eq!(ev.source_event_id, "evt_16zw770jnvito");
    assert_eq!(ev.agent, "claude_code");
    assert_eq!(ev.tokens.as_ref().unwrap().cache_read, 5);
    assert_eq!(
        ev.content_excerpt.as_deref(),
        Some("did some work on the ingest path")
    );
    assert_eq!(
        ev.seq,
        Some(128),
        "the source-log ordinal survives the wire"
    );
}

/// The instants only a producer INSIDE the call path can state. `ts` is
/// untouched by them; each of the two rides on its own, and a fixture that
/// carried them as `null` would be a different claim from omitting them.
#[test]
fn sdk_instants_accept_and_roundtrip_beside_ts() {
    let ev: RawEvent = roundtrip("raw_event_sdk_instants.json");
    assert_eq!(ev.ts, "2026-06-01T10:00:00.000Z");
    assert_eq!(ev.started_at.as_deref(), Some("2026-06-01T10:00:00.000Z"));
    assert_eq!(
        ev.first_token_at.as_deref(),
        Some("2026-06-01T10:00:00.140Z")
    );
    assert_eq!(ev.seq, None, "an SDK has no source log to be positioned in");
}

#[test]
fn raw_event_minimal_materializes_defaults() {
    let text = read("raw_event_minimal.json");
    let ev: RawEvent = serde_json::from_str(&text).unwrap();
    // Nullable-as-null present; defaults filled; optionals absent.
    assert!(ev.model.is_none());
    assert!(ev.tokens.is_none());
    assert!(ev.tool_calls.is_empty());
    assert!(ev.files_touched.is_empty());
    assert!(ev.content_excerpt.is_none());
    assert!(ev.seq.is_none());
    assert!(ev.started_at.is_none());
    assert!(ev.first_token_at.is_none());
    // Re-serialize: default containers present, optionals omitted.
    let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
    assert!(v.get("tool_calls").unwrap().is_object());
    assert!(v.get("files_touched").unwrap().is_array());
    assert!(v.get("model").unwrap().is_null());
    assert!(v.get("content_excerpt").is_none());
    assert!(v.get("seq").is_none());
    assert!(v.get("started_at").is_none());
    assert!(v.get("first_token_at").is_none());
}

#[test]
fn tool_call_and_action_accept_and_roundtrip() {
    let tc: ToolCallWire = roundtrip("tool_call.json");
    assert_eq!(tc.name, "Bash");
    assert_eq!(tc.signature_hash, "none");
    let action = tc.action.expect("action present");
    assert_eq!(action.surface, "shell");
    assert_eq!(action.executable.as_deref(), Some("kubectl"));
    assert_eq!(action.extractor, "shell.v3");
}

#[test]
fn segment_accepts_catchall_and_behavior() {
    let seg: Segment = roundtrip("segment.json");
    assert_eq!(seg.segment_id, "seg_bf79y774ikaf");
    // `pf_name` catchall counter survives the round-trip.
    assert_eq!(seg.redaction.extra.get("pf_name"), Some(&1));
    assert_eq!(seg.behavior.as_ref().unwrap().user_turns, 3);
    assert!(seg.abstract_embedding.is_none());
}

#[test]
fn segment_with_embedding_has_384_dims() {
    let seg: Segment = roundtrip("segment_with_embedding.json");
    assert_eq!(seg.abstract_embedding.as_ref().unwrap().len(), 384);
}

#[test]
fn ingest_batch_accepts_and_roundtrips() {
    let batch: IngestBatch = roundtrip("ingest_batch.json");
    assert_eq!(batch.events.len(), 3);
    // The zone the building machine was in — the one working-day fact that is
    // lost the moment a batch ships nothing but UTC. Both readings ride: the
    // IANA NAME (the durable fact) and the offset in force, in the two
    // spellings the wire has carried.
    assert_eq!(batch.utc_offset_minutes, Some(-420));
    assert_eq!(batch.tz.as_deref(), Some("America/Los_Angeles"));
    assert_eq!(batch.tz_offset_minutes, Some(-420));
    assert_eq!(batch.segments.len(), 1);
    assert_eq!(batch.tool_calls.len(), 1);
    assert_eq!(batch.summarizer_mode.as_deref(), Some("cloud"));
    assert_eq!(
        batch.session_titles.as_ref().unwrap()["11111111-1111-1111-1111-111111111111"],
        "Ingest retry matrix"
    );
    let anchors = batch.repo_anchors.as_ref().unwrap();
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0].slug, "acme/api");
    assert_eq!(anchors[0].host.as_deref(), Some("github.com"));
    assert_eq!(anchors[0].anchors.len(), 1);
    assert_eq!(anchors[0].anchors[0].pr_number, 421);
    assert_eq!(anchors[0].anchors[0].span_ms, Some(259_200_000));
    assert_eq!(anchors[0].anchors[0].commit_count, Some(7));
}

#[test]
fn heartbeat_accepts_and_roundtrips() {
    let hb: HeartbeatPayload = roundtrip("heartbeat.json");
    assert_eq!(hb.status, "scanning");
    assert_eq!(hb.progress_total, 12);
    assert_eq!(hb.daemon_version, "daemon-0.0.0");
    // Both readings of the zone: the durable NAME and the offset in force.
    assert_eq!(hb.timezone.as_deref(), Some("America/Los_Angeles"));
    assert_eq!(hb.utc_offset_minutes, Some(-420));
}

#[test]
fn clamp_vectors_match_ts() {
    let text = read("clamp.json");
    let cases: serde_json::Value = serde_json::from_str(&text).unwrap();
    for case in cases.as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let max = case["max_bytes"].as_u64().unwrap() as usize;
        let expected = case["expected"].as_str().unwrap();
        assert_eq!(
            clamp_utf8_bytes(input, max),
            expected,
            "clamp case {}",
            case["name"]
        );
    }
}
