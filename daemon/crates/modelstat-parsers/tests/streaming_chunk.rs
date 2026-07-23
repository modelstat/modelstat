//! Streaming chunk-boundary coverage — the M2 test-depth gap a completeness
//! audit flagged. The golden fixtures are ≤4 events, so the streaming parsers'
//! `PARSER_EVENT_CHUNK` (256) flush + backpressure NEVER fire there. The TS side
//! had a dedicated 600-event `streaming.test.ts`; this ports its intent: a
//! synthetic >256-event transcript that exercises the bounded-chunk contract the
//! scan loop relies on (a multi-hundred-MB transcript must never materialise as
//! one array).

use modelstat_parsers::claude_code::parse_claude_code_jsonl_streaming;
use modelstat_parsers::{parse_claude_code_jsonl, ParserContext, PARSER_EVENT_CHUNK};
use modelstat_wire::RawEvent;

#[test]
fn streaming_flushes_at_the_chunk_boundary_and_matches_collect() {
    let dir = std::env::temp_dir().join(format!("modelstat-stream-chunk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.jsonl");

    // 600 distinct user messages → 600 events (> the 256 chunk cap → ≥3 chunks).
    let n = 600usize;
    let mut body = String::with_capacity(n * 180);
    for i in 0..n {
        body.push_str(&format!(
            "{{\"type\":\"user\",\"uuid\":\"u{i}\",\"sessionId\":\"33333333-3333-3333-3333-333333333333\",\"timestamp\":\"2026-06-01T09:00:00.000Z\",\"cwd\":\"/repo\",\"message\":{{\"role\":\"user\",\"content\":\"msg {i}\"}}}}\n"
        ));
    }
    std::fs::write(&path, &body).unwrap();

    let ctx = ParserContext::new("dev1", path.to_string_lossy().into_owned());
    let collected = parse_claude_code_jsonl(&ctx).unwrap();
    assert!(
        collected.events.len() > PARSER_EVENT_CHUNK,
        "fixture must exceed the chunk cap to exercise the boundary (got {})",
        collected.events.len()
    );

    // Stream: record every chunk the sink receives.
    let mut chunk_sizes: Vec<usize> = Vec::new();
    let mut streamed: Vec<RawEvent> = Vec::new();
    let res = parse_claude_code_jsonl_streaming(&ctx, &mut |chunk: Vec<RawEvent>| {
        chunk_sizes.push(chunk.len());
        streamed.extend(chunk);
    })
    .unwrap();

    // Bounded-chunk contract: MORE THAN ONE chunk, NONE over the cap.
    assert!(
        chunk_sizes.len() > 1,
        "streaming must flush multiple chunks, got {}",
        chunk_sizes.len()
    );
    assert!(
        chunk_sizes.iter().all(|&c| c <= PARSER_EVENT_CHUNK),
        "a chunk exceeded PARSER_EVENT_CHUNK={PARSER_EVENT_CHUNK}: {chunk_sizes:?}"
    );
    // Equivalence STILL holds at scale + streaming never accumulates in-memory.
    assert!(
        res.events.is_empty(),
        "streaming must not accumulate events"
    );
    assert_eq!(
        streamed, collected.events,
        "streamed events must equal collected"
    );
    assert_eq!(res.tool_calls, collected.tool_calls);
    assert_eq!(res.stats, collected.stats);

    let _ = std::fs::remove_dir_all(&dir);
}
