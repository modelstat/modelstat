//! Golden parity — §4.3 (redaction floor). Every case's redacted text and the
//! three counts must equal what the TS wire floor `redact()` produced. A break
//! here is a privacy-contract regression, so the fixtures live in the shared
//! golden dir (under modelstat-wire) and both impls are pinned to them.

use modelstat_redact::{redact, FLOOR_REPLACEMENT_TEMPLATES};
use serde_json::Value;

fn golden() -> Value {
    // The golden dir lives under the sibling modelstat-wire crate.
    let path = format!(
        "{}/../modelstat-wire/tests/golden/redaction.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

#[test]
fn floor_order_matches_ts_catalogue() {
    let j = golden();
    let ts: Vec<&str> = j["floor_order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let rust: Vec<&str> = FLOOR_REPLACEMENT_TEMPLATES
        .iter()
        .map(|(n, _)| *n)
        .collect();
    assert_eq!(ts, rust, "SECRET_FLOOR name/order drifted from TS");
    assert_eq!(rust.len(), 18);
}

#[test]
fn every_case_matches_ts() {
    let j = golden();
    let mut checked = 0;
    for case in j["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        // The committed `input` carries a U+0000 after each credential prefix so
        // the raw fixture matches no secret scanner (GitHub push protection);
        // strip it to recover the real input the TS redactor saw.
        let input = case["input"].as_str().unwrap().replace('\u{0}', "");
        let repo_root = case["repo_root"].as_str();
        let r = redact(&input, repo_root);
        assert_eq!(
            r.text,
            case["text"].as_str().unwrap(),
            "text mismatch in case {name}"
        );
        assert_eq!(
            r.counts.secrets_found,
            case["secrets_found"].as_u64().unwrap(),
            "secrets_found in case {name}"
        );
        assert_eq!(
            r.counts.emails_redacted,
            case["emails_redacted"].as_u64().unwrap(),
            "emails_redacted in case {name}"
        );
        assert_eq!(
            r.counts.paths_redacted_absolute,
            case["paths_redacted_absolute"].as_u64().unwrap(),
            "paths_redacted_absolute in case {name}"
        );
        checked += 1;
    }
    assert!(checked >= 30, "expected the full case table, got {checked}");
}
