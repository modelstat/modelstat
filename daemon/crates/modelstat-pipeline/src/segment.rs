//! Segmentation (feature §18) — a port of the boundary detection in
//! `packages/daemon-core/src/pipeline/index.ts`.
//!
//! A session's turns (sorted by ts) are split into segments at the first of:
//! a ≥15-min time gap, ≥100 turns, ≥30-min duration, ≥12,000 accumulated excerpt
//! chars, or a cosine topic-distance > 0.35 between adjacent turn embeddings.
//! Single-turn slices are merged into the previous segment.
//!
//! The embedding surface is metadata-only (kind/agent/model/`tools:`/`files:`),
//! already redaction-safe. Embeddings are computed by the caller (candle BGE);
//! this module is the pure boundary math so it is exhaustively testable without a
//! model — a missing embedding just skips the cosine check for that pair.

use modelstat_wire::RawEvent;

use crate::prompts::{
    SEGMENT_MAX_CONTENT_CHARS, SEGMENT_MAX_DURATION_MS, SEGMENT_MAX_TURNS, SEGMENT_TIME_GAP_MS,
    SEGMENT_TOPIC_THRESHOLD,
};

/// The per-turn inputs the boundary detector needs.
#[derive(Debug, Clone)]
pub struct TurnMeta {
    /// Turn timestamp in epoch milliseconds (the caller parses `RawEvent.ts`).
    pub ts_ms: i64,
    /// `content_excerpt` length in UTF-16 units (JS `.length`) — feeds the
    /// accumulated-content-chars split.
    pub content_chars: usize,
    /// The turn's embedding, or empty when unavailable (cosine check skipped).
    pub embedding: Vec<f32>,
}

/// The metadata-only surface a turn is embedded from (§18). Port of `turnSurface`.
/// NOTE: `tool_calls` keys come out sorted (the wire schema is a `BTreeMap`),
/// where the TS used object insertion order — a benign divergence absorbed by the
/// PROCESSING_VERSION 16 replay (plan D13/R2).
pub fn turn_surface(e: &RawEvent) -> String {
    let mut parts: Vec<String> = vec![e.kind.clone(), e.agent.clone()];
    if let Some(model) = &e.model {
        if !model.is_empty() {
            parts.push(model.clone());
        }
    }
    if !e.tool_calls.is_empty() {
        let keys: Vec<&str> = e.tool_calls.keys().map(String::as_str).collect();
        parts.push(format!("tools:{}", keys.join(",")));
    }
    if !e.files_touched.is_empty() {
        parts.push(format!("files:{}", e.files_touched.len()));
    }
    parts.join(" ")
}

/// Build a [`TurnMeta`] from a sorted event + its (possibly empty) embedding.
pub fn turn_meta(e: &RawEvent, embedding: Vec<f32>) -> TurnMeta {
    let content_chars = e
        .content_excerpt
        .as_deref()
        .map(|s| s.encode_utf16().count())
        .unwrap_or(0);
    TurnMeta {
        ts_ms: parse_ts_ms(&e.ts),
        content_chars,
        embedding,
    }
}

/// Parse an RFC3339 timestamp to epoch millis (JS `Date.parse`). Best-effort:
/// unparseable → 0 (matches JS `NaN` degrading gracefully in the gap math — the
/// caller sorts first, so a bad ts only perturbs one pair).
pub fn parse_ts_ms(ts: &str) -> i64 {
    // A minimal, allocation-free RFC3339 → epoch-ms conversion covering the
    // shapes the parsers emit (`…Z` and `…±HH:MM`). Falls back to 0.
    chrono_parse(ts).unwrap_or(0)
}

fn chrono_parse(ts: &str) -> Option<i64> {
    // Avoid a chrono dep here: hand-parse `YYYY-MM-DDTHH:MM:SS(.fff)?(Z|±HH:MM)`.
    let bytes = ts.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| ts.get(a..b)?.parse::<i64>().ok();
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    // Optional `.fff` fractional seconds.
    let mut idx = 19;
    let mut millis_frac = 0i64;
    if bytes.get(19) == Some(&b'.') {
        let mut frac = String::new();
        idx = 20;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            if frac.len() < 3 {
                frac.push(bytes[idx] as char);
            }
            idx += 1;
        }
        while frac.len() < 3 {
            frac.push('0');
        }
        millis_frac = frac.parse().unwrap_or(0);
    }
    // Offset: `Z` or `±HH:MM`.
    let mut offset_min = 0i64;
    if let Some(&c) = bytes.get(idx) {
        if c == b'+' || c == b'-' {
            let oh = ts.get(idx + 1..idx + 3)?.parse::<i64>().ok()?;
            let om = ts.get(idx + 4..idx + 6)?.parse::<i64>().ok()?;
            offset_min = (oh * 60 + om) * if c == b'-' { -1 } else { 1 };
        }
    }
    let days = days_from_civil(year, month, day);
    let epoch_secs = days * 86_400 + hour * 3600 + min * 60 + sec - offset_min * 60;
    Some(epoch_secs * 1000 + millis_frac)
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Cosine distance `1 - cos(a,b)` over the shared prefix. Port of `cosineDistance`
/// — accumulates in f64 (the TS used JS `number`s); a zero denominator → 1.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for i in 0..n {
        let (ai, bi) = (a[i] as f64, b[i] as f64);
        dot += ai * bi;
        na += ai * ai;
        nb += bi * bi;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom > 0.0 {
        1.0 - dot / denom
    } else {
        1.0
    }
}

/// Split pre-sorted turns into segments, returning each segment's turn indices.
/// Port of the boundary loop + singleton merge in `buildForOneSession`.
pub fn segment_turns(turns: &[TurnMeta]) -> Vec<Vec<usize>> {
    if turns.is_empty() {
        return Vec::new();
    }

    let mut boundaries: Vec<usize> = Vec::new();
    let mut run_start = 0usize;
    let mut run_start_ms = turns[0].ts_ms;
    let mut run_chars = turns[0].content_chars;

    for i in 1..turns.len() {
        let prev = &turns[i - 1];
        let cur = &turns[i];
        let gap = cur.ts_ms - prev.ts_ms;
        let run_ms = cur.ts_ms - run_start_ms;
        let turns_in_run = i - run_start;

        let split = gap >= SEGMENT_TIME_GAP_MS
            || turns_in_run >= SEGMENT_MAX_TURNS
            || run_ms >= SEGMENT_MAX_DURATION_MS
            || run_chars >= SEGMENT_MAX_CONTENT_CHARS
            || (!prev.embedding.is_empty()
                && !cur.embedding.is_empty()
                && cosine_distance(&prev.embedding, &cur.embedding) > SEGMENT_TOPIC_THRESHOLD);

        if split {
            boundaries.push(i);
            run_start = i;
            run_start_ms = cur.ts_ms;
            run_chars = cur.content_chars;
        } else {
            run_chars += cur.content_chars;
        }
    }
    boundaries.push(turns.len());

    // Materialise contiguous slices, then merge single-turn slices into the
    // previous segment (avoid per-turn fragments).
    let mut merged: Vec<Vec<usize>> = Vec::new();
    let mut prev = 0usize;
    for b in boundaries {
        let slice: Vec<usize> = (prev..b).collect();
        prev = b;
        if slice.len() == 1 && !merged.is_empty() {
            merged.last_mut().unwrap().extend(slice);
        } else {
            merged.push(slice);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(ts_ms: i64, chars: usize, emb: Vec<f32>) -> TurnMeta {
        TurnMeta {
            ts_ms,
            content_chars: chars,
            embedding: emb,
        }
    }

    #[test]
    fn parse_ts_matches_known_epochs() {
        assert_eq!(parse_ts_ms("2026-06-26T23:53:20.000Z"), 1_782_518_000_000);
        assert_eq!(parse_ts_ms("2026-06-01T10:00:00.000Z"), 1_780_308_000_000);
        // Offset form.
        assert_eq!(
            parse_ts_ms("2026-06-01T12:00:00.000+02:00"),
            parse_ts_ms("2026-06-01T10:00:00.000Z")
        );
    }

    #[test]
    fn cosine_distance_bounds() {
        assert!((cosine_distance(&[1.0, 0.0], &[1.0, 0.0]) - 0.0).abs() < 1e-9);
        assert!((cosine_distance(&[1.0, 0.0], &[0.0, 1.0]) - 1.0).abs() < 1e-9);
        assert_eq!(cosine_distance(&[0.0, 0.0], &[1.0, 1.0]), 1.0); // zero denom → 1
    }

    #[test]
    fn time_gap_splits() {
        let turns = vec![
            turn(0, 10, vec![]),
            turn(60_000, 10, vec![]),               // +1 min → same segment
            turn(60_000 + 16 * 60_000, 10, vec![]), // +16 min → split
        ];
        // Third turn is a singleton after the split → merged into the second's
        // segment? No: the split makes [0,1] and [2]; [2] is a singleton merged
        // back into [0,1].
        let segs = segment_turns(&turns);
        assert_eq!(segs, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn topic_shift_splits_when_embeddings_diverge() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0]; // cosine distance 1.0 > 0.35
        let turns = vec![
            turn(0, 10, a.clone()),
            turn(1000, 10, a.clone()),
            turn(2000, 10, b.clone()), // topic shift → boundary before index 2
            turn(3000, 10, b.clone()),
        ];
        let segs = segment_turns(&turns);
        assert_eq!(segs, vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn max_turns_caps_a_segment() {
        // 150 rapid same-topic turns → split at 100.
        let emb = vec![1.0f32, 0.0];
        let turns: Vec<TurnMeta> = (0..150)
            .map(|i| turn(i as i64 * 1000, 1, emb.clone()))
            .collect();
        let segs = segment_turns(&turns);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].len(), 100);
        assert_eq!(segs[1].len(), 50);
    }

    #[test]
    fn content_chars_cap_splits_long_on_topic_runs() {
        let emb = vec![1.0f32, 0.0];
        // Each turn 5000 chars: after 3 turns run_chars ≥ 12000 → split.
        let turns: Vec<TurnMeta> = (0..5)
            .map(|i| turn(i as i64 * 1000, 5000, emb.clone()))
            .collect();
        let segs = segment_turns(&turns);
        // run: turn0(5000) turn1(+5000=10000) turn2(+5000=15000 but the check is
        // BEFORE adding cur: at i=3 run_chars=15000≥12000 → split). Verify ≥2 segs.
        assert!(
            segs.len() >= 2,
            "expected a content-cap split, got {segs:?}"
        );
    }

    #[test]
    fn turn_surface_is_metadata_only() {
        use modelstat_wire::{RawEvent, TokenUsage};
        let mut ev = RawEvent {
            content_bytes: None,
            source_event_id: "e".into(),
            ts: "2026-01-01T00:00:00Z".into(),
            kind: "assistant_message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: Some("claude-opus-4-8".into()),
            session_id: "s".into(),
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: Some(TokenUsage::default()),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: vec!["a".into(), "b".into()],
            content_excerpt: Some("secret content".into()),
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        };
        ev.tool_calls.insert("Bash".into(), 2);
        let s = turn_surface(&ev);
        assert_eq!(
            s,
            "assistant_message claude_code claude-opus-4-8 tools:Bash files:2"
        );
        assert!(!s.contains("secret")); // never embeds content
    }
}
