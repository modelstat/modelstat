//! Golden parity — §4.1 (ids), §4.2 (paramShape), and the enum arrays.
//!
//! `ids.json`, `param_shape.json` and `enums.json` are generated from the LIVE
//! TypeScript (`packages/core`, shipped by the extension and the MCP server), so
//! a failure there means the two implementations drifted — a wire-contract break.
//!
//! `device.json` is different: its TypeScript implementation lived in the
//! retired daemon and is deleted, so those vectors are a FROZEN contract that
//! nothing regenerates. A failure there means the Rust derivation moved, which
//! would re-enroll every existing device.

use modelstat_wire::device::{
    device_uuid_from_machine_key, intended_device_uuid, machine_key_hash,
};
use modelstat_wire::ids::{segment_id, source_event_id, tc_fallback_id, EventSource};
use modelstat_wire::param_shape::param_shape;
use serde_json::Value;

fn golden(name: &str) -> Value {
    let path = format!("{}/tests/golden/{}", env!("CARGO_MANIFEST_DIR"), name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

#[test]
fn source_event_id_matches_ts() {
    let j = golden("ids.json");
    for case in j["source_event_id"].as_array().unwrap() {
        let device = case["device_id"].as_str().unwrap();
        let s = &case["source"];
        let expected = case["expected"].as_str().unwrap();
        let src = match s["type"].as_str().unwrap() {
            "file" => EventSource::File {
                file: s["file"].as_str().unwrap(),
                byte_offset: s["byte_offset"].as_u64().unwrap(),
            },
            "line_uuid" => EventSource::LineUuid {
                line_uuid: s["line_uuid"].as_str().unwrap(),
            },
            "web" => EventSource::Web {
                host: s["host"].as_str().unwrap(),
                conversation_id: s["conversation_id"].as_str().unwrap(),
                message_id: s["message_id"].as_str().unwrap(),
            },
            other => panic!("unknown source type {other}"),
        };
        assert_eq!(source_event_id(device, &src), expected, "case {s}");
    }

    // Legacy 3-arg form hashes identically to the object form.
    let le = &j["legacy_equivalence"];
    assert_eq!(le["three_arg"], le["object_form"]);
}

#[test]
fn segment_id_matches_ts() {
    let j = golden("ids.json");
    for case in j["segment_id"].as_array().unwrap() {
        let ids: Vec<String> = case["source_event_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let got = segment_id(
            case["session_id"].as_str().unwrap(),
            case["started_at_ms"].as_i64().unwrap(),
            case["ended_at_ms"].as_i64().unwrap(),
            &ids,
        );
        assert_eq!(got, case["expected"].as_str().unwrap(), "case {case}");
    }
}

#[test]
fn tc_fallback_id_matches_ts() {
    let j = golden("ids.json");
    for case in j["tc_fallback_id"].as_array().unwrap() {
        let got = tc_fallback_id(
            case["source_event_id"].as_str().unwrap(),
            case["call_index"].as_u64().unwrap(),
        );
        assert_eq!(got, case["expected"].as_str().unwrap(), "case {case}");
    }
}

#[test]
fn device_derivations_match_frozen_vectors() {
    let j = golden("device.json");
    for case in j["machine_key_hash"].as_array().unwrap() {
        let got = machine_key_hash(case["raw"].as_str().unwrap());
        assert_eq!(
            got,
            case["expected"].as_str().unwrap(),
            "raw {}",
            case["raw"]
        );
    }
    for case in j["device_uuid"].as_array().unwrap() {
        let got = device_uuid_from_machine_key(case["key"].as_str().unwrap());
        assert_eq!(got, case["expected"].as_str().unwrap());
    }
    for case in j["device_uuid_salted"].as_array().unwrap() {
        let got = intended_device_uuid(
            case["machine_key"].as_str().unwrap(),
            Some(case["salt"].as_str().unwrap()),
        );
        assert_eq!(got, case["expected"].as_str().unwrap());
    }
}

#[test]
fn param_shape_matches_ts() {
    let j = golden("param_shape.json");
    for case in j.as_array().unwrap() {
        let got = param_shape(case["input"].as_str().unwrap());
        assert_eq!(
            got,
            case["expected"].as_str().unwrap(),
            "input {}",
            case["input"]
        );
    }
}

#[test]
fn enum_arrays_match_ts() {
    use modelstat_wire::enums::*;
    let j = golden("enums.json");
    let check = |key: &str, arr: &[&str]| {
        let ts: Vec<&str> = j[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(ts, arr, "enum {key} drifted from TS");
    };
    check("agents", AGENTS);
    check("providers", PROVIDERS);
    check("event_kinds", EVENT_KINDS);
    check("tool_call_statuses", TOOL_CALL_STATUSES);
    check("os_families", OS_FAMILIES);
    check("daemon_phases", DAEMON_PHASES);
    check("install_methods", INSTALL_METHODS);
    check("identity_owner_scopes", IDENTITY_OWNER_SCOPES);
    check("classification_confidence", CLASSIFICATION_CONFIDENCE);
}
