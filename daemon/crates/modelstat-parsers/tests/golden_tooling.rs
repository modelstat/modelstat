//! Tooling golden parity — §4.2 (`extractExecutable`, `normalizeToolName`,
//! `splitObservedToolName`), against the frozen vectors in
//! `modelstat-wire/tests/golden/`.
//!
//! These vectors were produced by the TypeScript implementations before that
//! port was retired. Nothing regenerates them now — and that is deliberate: a
//! fixture the implementation can rewrite pins nothing. They are a frozen
//! contract, and this suite is the gate. `shell.v3` executable extraction and
//! tool-name normalization decide taxonomy leaves, per-tool rollups and facet
//! values, so a silent drift here re-buckets history.

use modelstat_parsers::tool_action::extract_executable;
use modelstat_parsers::{normalize_tool_name, split_observed_tool_name};
use serde_json::Value;

fn golden(name: &str) -> Value {
    let path = format!(
        "{}/../modelstat-wire/tests/golden/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

#[test]
fn shell_executable_matches_frozen_vectors() {
    let j = golden("shell_executable.json");
    for case in j.as_array().unwrap() {
        let command = case["command"].as_str().unwrap();
        let got = extract_executable(command);
        assert_eq!(
            got,
            case["expected"].as_str().unwrap(),
            "command {command:?}"
        );
    }
}

#[test]
fn normalize_tool_name_matches_frozen_vectors() {
    let j = golden("tool_name.json");
    for case in j["normalize"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let got = normalize_tool_name(input);
        assert_eq!(got, case["expected"].as_str().unwrap(), "input {input:?}");
    }
}

#[test]
fn split_observed_tool_name_matches_frozen_vectors() {
    let j = golden("tool_name.json");
    for case in j["split"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let (server, name) = split_observed_tool_name(input);
        assert_eq!(server, case["server"].as_str().unwrap(), "server {input:?}");
        assert_eq!(name, case["name"].as_str().unwrap(), "name {input:?}");
    }
}
