//! Does the REAL on-device NER model still redact a LONG turn?
//!
//! SPEC 0005 made turns verbatim (cap 262144 chars), so this stopped being a
//! hypothetical: BERT carries 512 learned positions, and `ner_redact` treats an
//! unavailable answer as pass-through. If a long sequence errors, the turn ships
//! to the cloud UNREDACTED — the fail-closed probe (`ner_active`) would not catch
//! it, because that probe uses one short sentinel.
//!
//! Needs the downloaded weights; skips (loudly) without them.

#![cfg(feature = "candle")]

use std::path::PathBuf;

#[test]
fn a_long_turn_is_still_redacted() {
    let dir =
        PathBuf::from(std::env::var("HOME").unwrap()).join(".modelstat/models/hf/bert-base-NER");
    if !dir.join("config.json").exists() {
        eprintln!("SKIP: no weights at {}", dir.display());
        return;
    }
    let model = modelstat_redact::ner::CandleNer::load(&dir).expect("load weights");
    let name = "Escalate the incident to Katherine Johnson at Globex Corporation.";
    let mut failures = Vec::new();
    for reps in [1usize, 20, 60, 200, 1000] {
        let text = format!(
            "{}{name}",
            "the quick brown fox jumps over the lazy dog ".repeat(reps)
        );
        let started = std::time::Instant::now();
        let out = modelstat_redact::ner_redact(&model, &text);
        let leaked = out.text.contains("Katherine Johnson");
        eprintln!(
            "chars={:>7} redacted={:<5} {:>8.0}ms",
            text.len(),
            !leaked,
            started.elapsed().as_secs_f64() * 1000.0
        );
        if leaked {
            failures.push(text.len());
        }
    }
    assert!(
        failures.is_empty(),
        "these turn sizes shipped the name UNREDACTED: {failures:?}"
    );
}
