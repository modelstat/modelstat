//! The redactor against the REAL weights.
//!
//! Everything else about layer 2 is tested on fakes; this is the only place the
//! actual model is asked to do the actual job, and it exists because the two bugs
//! that reached production were both invisible to fakes: a size limit that made
//! long turns pass through unscrubbed, and spans spliced mid-word.
//!
//! Needs the downloaded weights; skips loudly without them, so CI stays green on a
//! machine with no model cache while a developer still gets the check.

#![cfg(feature = "onnx")]

use std::path::PathBuf;
use std::time::Instant;

use modelstat_redact::privacy_filter::PrivacyFilter;
use modelstat_redact::{ner_active, ner_redact_checked};

fn model_dir() -> Option<PathBuf> {
    [
        std::env::var("MODELSTAT_REDACTOR_MODEL_DIR")
            .ok()
            .map(PathBuf::from),
        Some(
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".modelstat/models/hf/privacy-filter"),
        ),
    ]
    .into_iter()
    .flatten()
    .find(|dir| dir.join("onnx/model_q4.onnx").exists())
}

/// No marker may sit against an alphanumeric character — a fragment of a redacted
/// word is both corruption and a leak of the part that survived.
fn assert_no_word_fragments(out: &str) {
    let chars: Vec<char> = out.chars().collect();
    let open: Vec<char> = "[REDACTED:".chars().collect();
    for i in 0..chars.len() {
        if chars[i..].starts_with(&open) {
            if i > 0 {
                assert!(
                    !chars[i - 1].is_alphanumeric(),
                    "marker starts inside a word:\n  {out}"
                );
            }
            let close = i + chars[i..].iter().position(|&c| c == ']').unwrap() + 1;
            if close < chars.len() {
                assert!(
                    !chars[close].is_alphanumeric(),
                    "marker ends inside a word:\n  {out}"
                );
            }
        }
    }
}

#[test]
fn it_redacts_what_must_never_ship() {
    let Some(dir) = model_dir() else {
        eprintln!("SKIP: no privacy-filter weights");
        return;
    };
    let m = PrivacyFilter::load(&dir).expect("load");

    // The liveness gate every egress path consults.
    assert!(ner_active(&m), "the sentinel person must be scrubbed");

    // (input, what must be gone, which marker must appear)
    let cases: [(&str, &str, &str); 4] = [
        (
            "Escalate the incident to Katherine Johnson please",
            "Katherine Johnson",
            "PRIVATE_PERSON",
        ),
        (
            "email me at katherine.johnson@globex.io when done",
            "katherine.johnson@globex.io",
            "PRIVATE_EMAIL",
        ),
        (
            "use key sk-proj-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6 for staging",
            "sk-proj-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
            "SECRET",
        ),
        (
            "call me on +1 415 555 0132 tonight",
            "555 0132",
            "PRIVATE_PHONE",
        ),
    ];
    for (input, must_be_gone, marker) in cases {
        let out = ner_redact_checked(&m, input)
            .expect("the model answered")
            .text;
        eprintln!("  {out}");
        assert!(
            !out.contains(must_be_gone),
            "{must_be_gone} survived: {out}"
        );
        assert!(out.contains(marker), "expected {marker} in: {out}");
        assert_no_word_fragments(&out);
    }
}

/// Technical prose must come through INTACT. The model this replaced redacted
/// `eRPC`, `Bugbot`, `Compose` and `ClickHouse` as organisations — 27,130 stored
/// messages worth — which cost the prompt analytics and protected nothing.
#[test]
fn it_leaves_technical_prose_alone() {
    let Some(dir) = model_dir() else {
        eprintln!("SKIP: no privacy-filter weights");
        return;
    };
    let m = PrivacyFilter::load(&dir).expect("load");
    // "Reviewed by Cursor Bugbot" is deliberately NOT here: the model reads it as a
    // person, which in "Reviewed by X" position is a reasonable mistake, and no
    // operating point fixes it (measured at bias 0.0/1.5/3.0). We accept that cost
    // — a name-shaped false positive is the side to err on — and pin the rest.
    for (text, keep) in [
        ("Error: eRPC Request failed for erpc-proxy", "eRPC"),
        (
            "a Compose app failed the p2p-compliance-oracle check",
            "Compose",
        ),
        ("the ClickHouse historical backfill ran twice", "ClickHouse"),
        (
            "deploy modelstat-daemon to the Hetzner box",
            "modelstat-daemon",
        ),
    ] {
        let out = ner_redact_checked(&m, text).expect("answered").text;
        eprintln!("  {out}");
        assert!(out.contains(keep), "{keep} was redacted away: {out}");
        assert_no_word_fragments(&out);
    }
}

/// Long turns, which is where the previous model failed outright (it errored past
/// 512 positions, and that error read as "no model" — pass-through). Also the
/// before/after timing table's source.
#[test]
fn long_turns_are_redacted_and_fast() {
    let Some(dir) = model_dir() else {
        eprintln!("SKIP: no privacy-filter weights");
        return;
    };
    let m = PrivacyFilter::load(&dir).expect("load");
    let name = "Escalate the incident to Katherine Johnson at Globex Corporation.";
    let filler = "the quick brown fox jumps over the lazy dog ";
    let _ = ner_redact_checked(&m, "warm up the session");
    for reps in [1usize, 20, 60, 200, 1000] {
        let text = format!("{}{name}", filler.repeat(reps));
        let started = Instant::now();
        let out = ner_redact_checked(&m, &text).expect("answered").text;
        eprintln!(
            "  chars={:>7} {:>8.0}ms redacted={}",
            text.len(),
            started.elapsed().as_secs_f64() * 1000.0,
            !out.contains("Katherine Johnson")
        );
        assert!(
            !out.contains("Katherine Johnson"),
            "a {}-char turn shipped the name",
            text.len()
        );
        assert_no_word_fragments(&out);
    }
}

/// What the recall bias BUYS and what it COSTS, on the same inputs. The operating
/// point is a product decision — "nothing sent unredacted" — so this exists to
/// keep it honest rather than assumed.
#[test]
fn the_operating_point_is_measured_not_assumed() {
    let Some(dir) = model_dir() else {
        eprintln!("SKIP: no privacy-filter weights");
        return;
    };
    // (text, the substring that MUST disappear) — real PII.
    let must_catch: [(&str, &str); 5] = [
        ("Escalate to Katherine Johnson please", "Katherine Johnson"),
        (
            "mail katherine.johnson@globex.io today",
            "katherine.johnson@globex.io",
        ),
        (
            "key sk-proj-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6 here",
            "sk-proj-A1b2",
        ),
        ("call +1 415 555 0132 tonight", "555 0132"),
        ("ship to 1 Infinite Loop, Cupertino", "Infinite Loop"),
    ];
    // (text, the substring that SHOULD survive) — technical prose, not private.
    let should_keep: [(&str, &str); 5] = [
        ("Error: eRPC Request failed for erpc-proxy", "eRPC"),
        ("a Compose app failed the oracle check", "Compose"),
        ("the ClickHouse historical backfill ran twice", "ClickHouse"),
        ("Reviewed by Cursor Bugbot 36 minutes ago", "Bugbot"),
        (
            "deploy modelstat-daemon to the Hetzner box",
            "modelstat-daemon",
        ),
    ];
    for bias in [0.0f32, 1.5, 3.0] {
        let m = PrivacyFilter::with_recall_bias(&dir, bias).expect("load");
        let caught = must_catch
            .iter()
            .filter(|(t, gone)| {
                !ner_redact_checked(&m, t)
                    .expect("answered")
                    .text
                    .contains(gone)
            })
            .count();
        let kept = should_keep
            .iter()
            .filter(|(t, keep)| {
                ner_redact_checked(&m, t)
                    .expect("answered")
                    .text
                    .contains(keep)
            })
            .count();
        eprintln!(
            "  bias={bias:<4} PII caught {caught}/{}  technical prose kept {kept}/{}",
            must_catch.len(),
            should_keep.len()
        );
        if bias >= 1.5 {
            assert_eq!(
                caught,
                must_catch.len(),
                "at the shipped operating point every one of these must be caught"
            );
        }
    }
}
