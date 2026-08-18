//! Parser golden parity — §4.4. Reconstructs the exact `/tmp/modelstat-fixtures`
//! tree from the committed inputs, runs the Rust parsers against those canonical
//! paths, and asserts byte-for-byte equal RawEvent / ToolCallDraft output against
//! the goldens in `modelstat-wire/tests/golden/parsers/`.
//!
//! These goldens are REGENERATED from the Rust (`REGEN_GOLDENS=1 … regen`): since
//! SPEC 0005 the Rust parsers deliberately supersede the retired TS port, so TS
//! is not their oracle. Reproducibility is the gate — CI regenerates and runs
//! `git diff --exit-code`.
//!
//! The fixed base path is load-bearing: `source_file` and the path-derived
//! `source_event_id`s are stable only when the inputs live at
//! `/tmp/modelstat-fixtures/...` (the generator's documented contract). So this
//! suite is a single Unix-gated test that owns that path.

#![cfg(unix)]

use std::path::Path;

use modelstat_parsers::types::{ParseStats, ToolCallDraft};
use modelstat_parsers::{
    parse_claude_code_jsonl, parse_codex_rollout, parse_cursor_tracking_db, parse_pi_session,
    ParserContext,
};
use modelstat_wire::RawEvent;
use serde::Deserialize;
use serde_json::Value;

const BASE: &str = "/tmp/modelstat-fixtures";

fn golden_dir() -> String {
    format!(
        "{}/../modelstat-wire/tests/golden/parsers",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn tree_dir() -> String {
    format!("{}/tests/fixtures/tree", env!("CARGO_MANIFEST_DIR"))
}

/// Recreate `/tmp/modelstat-fixtures` byte-for-byte from the committed inputs,
/// ONCE per process.
///
/// The fixed base path is shared by every test in this binary and `cargo test`
/// runs them on several threads, so a per-test wipe-and-copy has one test
/// deleting the tree another is mid-copy into — an `fs::copy` that fails on a
/// path that existed a microsecond ago. Doing it once behind a `OnceLock` keeps
/// the single-owner property the fixed path needs while letting any number of
/// tests depend on the tree.
fn materialize_tree() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let src_root = tree_dir();
        let _ = std::fs::remove_dir_all(BASE);
        copy_tree(Path::new(&src_root), Path::new(BASE));
    });
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

fn golden(name: &str) -> Value {
    let path = format!("{}/{name}", golden_dir());
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap()
}

fn events_of(g: &Value) -> Vec<RawEvent> {
    serde_json::from_value(g.get("events").cloned().unwrap_or(Value::Array(vec![]))).unwrap()
}

fn tool_calls_of(g: &Value) -> Vec<ToolCallDraft> {
    serde_json::from_value(g.get("toolCalls").cloned().unwrap_or(Value::Array(vec![]))).unwrap()
}

/// A parse context for a machine whose agent is logged in with a flat plan.
///
fn ctx(source_file: &str) -> ParserContext {
    ParserContext::new("dev_1", source_file)
}

fn ctx_api(source_file: &str) -> ParserContext {
    ParserContext::new("dev_1", source_file)
}

#[derive(Deserialize, PartialEq, Debug)]
struct ScriptCtx {
    external_call_id: String,
    command: String,
    cwd: Option<String>,
}

#[test]
fn parser_golden_parity() {
    materialize_tree();

    // 1. Claude basic — events, tool calls, script contexts, stats.
    {
        let file = format!("{BASE}/claude/11111111-1111-1111-1111-111111111111.jsonl");
        let res = parse_claude_code_jsonl(&ctx(&file)).unwrap();
        let g = golden("claude_basic.json");
        assert_eq!(res.events, events_of(&g), "claude_basic events");
        assert_eq!(res.tool_calls, tool_calls_of(&g), "claude_basic toolCalls");
        let stats: ParseStats = serde_json::from_value(g["stats"].clone()).unwrap();
        assert_eq!(res.stats, stats, "claude_basic stats");
        // script contexts (local-only; compared via an ad-hoc struct).
        let want_sc: Vec<ScriptCtx> = serde_json::from_value(g["scriptContexts"].clone()).unwrap();
        let got_sc: Vec<ScriptCtx> = res
            .script_contexts
            .iter()
            .map(|c| ScriptCtx {
                external_call_id: c.external_call_id.clone(),
                command: c.command.clone(),
                cwd: c.cwd.clone(),
            })
            .collect();
        assert_eq!(got_sc, want_sc, "claude_basic scriptContexts");
    }

    // 2. Claude synthetic — every line is an ancestor-sessioned resume copy whose
    //    ancestor (`1111…`) exists in the same dir ⇒ all dropped, 0 events.
    {
        let file = format!("{BASE}/claude/22222222-2222-2222-2222-222222222222.jsonl");
        let res = parse_claude_code_jsonl(&ctx(&file)).unwrap();
        let g = golden("claude_synthetic.json");
        assert_eq!(res.events, events_of(&g), "claude_synthetic events");
        assert!(res.events.is_empty());
    }

    // 3. Claude resume copy — the ancestor-sessioned line dropped, native kept.
    {
        let file = format!(
            "{BASE}/claude-resume/projects/myproj/44444444-4444-4444-4444-444444444444.jsonl"
        );
        let res = parse_claude_code_jsonl(&ctx(&file)).unwrap();
        let g = golden("claude_resume_copy.json");
        assert_eq!(res.events, events_of(&g), "claude_resume_copy events");
    }

    // 3b. Claude Desktop — the SAME Claude Code format, discovered under the
    //     desktop app's data dir and stamped with the host's agent label.
    {
        let file = format!("{BASE}/claude-desktop/ac0e34b8-76ab-4d62-bd0c-c67ed97bf5c0.jsonl");
        let res = parse_claude_code_jsonl(
            &ctx(&file).with_agent_label(Some("claude_desktop".to_string())),
        )
        .unwrap();
        let g = golden("claude_desktop.json");
        assert_eq!(res.events, events_of(&g), "claude_desktop events");
        assert_eq!(
            res.tool_calls,
            tool_calls_of(&g),
            "claude_desktop toolCalls"
        );
        assert!(
            res.events.iter().all(|e| e.agent == "claude_desktop"),
            "the host's label replaces the parser's own name"
        );
        assert!(
            res.tool_calls.iter().all(|c| c.agent == "claude_desktop"),
            "tool calls carry it too, or a session's calls and events disagree"
        );
    }

    // 4. Codex — disjoint token buckets, tool call anchored to its own line.
    {
        let file = format!(
            "{BASE}/codex/rollout-2026-08-05T13-58-57-019fd1ca-816d-7af2-9332-a6db0bfc4d25.jsonl"
        );
        let res = parse_codex_rollout(&ctx(&file)).unwrap();
        let g = golden("codex_basic.json");
        assert_eq!(res.events, events_of(&g), "codex events");
        assert_eq!(res.tool_calls, tool_calls_of(&g), "codex toolCalls");
    }

    // 4b. Codex fork replay — a subagent/resume rollout opens with its OWN
    //     `session_meta`, then replays the ancestor's whole history with the
    //     timestamps rewritten to the fork moment, then does its own new work.
    //     Two independent facts, and the fix for either breaks the other if
    //     they are conflated:
    //       · the fork is its OWN SESSION, named by its filename — the ancestor
    //         `session_meta` it replays is an ancestor pointer, not an identity;
    //       · every replayed round trip still lands on the SAME source_event_id
    //         the ancestor's own file produced, so the store collapses it
    //         instead of billing the conversation twice. The fork's new turn
    //         must NOT.
    {
        let anc = format!(
            "{BASE}/codex/rollout-2026-08-05T13-58-57-019fd1ca-816d-7af2-9332-a6db0bfc4d25.jsonl"
        );
        let fork = format!(
            "{BASE}/codex/rollout-2026-08-05T14-41-18-019fd1d5-2a4c-7bd1-9f03-1c7e5a90b442.jsonl"
        );
        let anc_res = parse_codex_rollout(&ctx(&anc)).unwrap();
        let fork_res = parse_codex_rollout(&ctx(&fork)).unwrap();

        // The session each file's events belong to is the file itself. Taking
        // the replayed declaration instead bound 447 of 485 real rollout files
        // to one session of 7.3M events — 64% of the events table, past every
        // processing ceiling, so it produced no tasks and no attribution.
        for (res, want) in [
            (&anc_res, "019fd1ca-816d-7af2-9332-a6db0bfc4d25"),
            (&fork_res, "019fd1d5-2a4c-7bd1-9f03-1c7e5a90b442"),
        ] {
            assert!(
                !res.events.is_empty() && res.events.iter().all(|e| e.session_id == want),
                "every event belongs to the rollout its filename names ({want})"
            );
        }

        let ids = |r: &modelstat_parsers::ParseResult| -> Vec<String> {
            r.events
                .iter()
                .filter(|e| e.kind == "assistant_message")
                .map(|e| e.source_event_id.clone())
                .collect()
        };
        let anc_ids = ids(&anc_res);
        let fork_ids = ids(&fork_res);
        assert_eq!(anc_ids.len(), 2, "the ancestor has two round trips");
        assert_eq!(
            fork_ids.len(),
            3,
            "the fork replays both, adds one of its own, and its restated \
             closing counter is not a fourth round trip"
        );
        assert_eq!(
            fork_ids[..2],
            anc_ids[..],
            "a replayed round trip keeps the ancestor's event id — the copy is \
             the SAME work, and a fresh id per fork file is what billed one \
             conversation 51 times"
        );
        assert!(
            !anc_ids.contains(&fork_ids[2]),
            "the fork's own new turn is new work and keeps its own id"
        );
        // The whole replayed prefix must be free, or the conversation still
        // double-counts — just more slowly.
        let replayed: u64 = fork_res
            .events
            .iter()
            .filter(|e| anc_ids.contains(&e.source_event_id))
            .filter_map(|e| e.tokens.as_ref())
            .map(|t| t.input + t.output + t.cache_read + t.cache_creation + t.reasoning)
            .sum();
        let anc_total: u64 = anc_res
            .events
            .iter()
            .filter_map(|e| e.tokens.as_ref())
            .map(|t| t.input + t.output + t.cache_read + t.cache_creation + t.reasoning)
            .sum();
        assert_eq!(
            replayed, anc_total,
            "the replay carries the ancestor's tokens verbatim, so collapsing \
             on the id is what keeps them counted once"
        );
    }

    // 5. pi — git slug from cwd on the assistant event, tokens mapping.
    {
        let file = format!(
            "{BASE}/pi/2026-06-26T23-53-00-262Z_019f0659-dc65-7969-af42-5dc1ced6232a.jsonl"
        );
        let res = parse_pi_session(&ctx_api(&file)).unwrap();
        let g = golden("pi_basic.json");
        assert_eq!(res.events, events_of(&g), "pi events");
        assert_eq!(res.tool_calls, tool_calls_of(&g), "pi toolCalls");
    }

    // 6. Cursor — SQLite rows → assistant events, byte offset null.
    {
        let file = format!("{BASE}/cursor/state.vscdb");
        let res = parse_cursor_tracking_db(&ctx(&file)).unwrap();
        let g = golden("cursor_basic.json");
        assert_eq!(res.events, events_of(&g), "cursor events");
        assert!(res.tool_calls.is_empty(), "cursor toolCalls empty");
    }

    // 7. Streaming-mode equivalence (M2 AC): each line-based parser must produce
    //    byte-identical events/tool_calls/stats whether it collects or streams in
    //    bounded chunks. Cursor is a row-set (not streamed) and is exempt.
    {
        use modelstat_parsers::claude_code::parse_claude_code_jsonl_streaming;
        use modelstat_parsers::codex::parse_codex_rollout_streaming;
        use modelstat_parsers::pi::parse_pi_session_streaming;

        let claude = format!("{BASE}/claude/11111111-1111-1111-1111-111111111111.jsonl");
        assert_stream_matches(
            parse_claude_code_jsonl(&ctx(&claude)).unwrap(),
            |emit| parse_claude_code_jsonl_streaming(&ctx(&claude), emit).unwrap(),
            "claude",
        );

        let codex = format!(
            "{BASE}/codex/rollout-2026-08-05T13-58-57-019fd1ca-816d-7af2-9332-a6db0bfc4d25.jsonl"
        );
        assert_stream_matches(
            parse_codex_rollout(&ctx(&codex)).unwrap(),
            |emit| parse_codex_rollout_streaming(&ctx(&codex), emit).unwrap(),
            "codex",
        );

        let pi = format!(
            "{BASE}/pi/2026-06-26T23-53-00-262Z_019f0659-dc65-7969-af42-5dc1ced6232a.jsonl"
        );
        assert_stream_matches(
            parse_pi_session(&ctx_api(&pi)).unwrap(),
            |emit| parse_pi_session_streaming(&ctx_api(&pi), emit).unwrap(),
            "pi",
        );
    }
}

/// What the REAL fixtures were silently dropping before anything counted it.
///
/// Every kind below is real agent output, not an invented shape: four record
/// types Claude Desktop writes and two codex event kinds, none of which any
/// parser arm had ever mentioned. They went to a bare `continue`, so the scan
/// reported success and nothing anywhere said a dialect had gone unread — the
/// exact failure that hid Cursor's schema move for weeks.
///
/// Pinned here so a parser that starts UNDERSTANDING one of these has to say so
/// by editing this list, and a parser that starts dropping something new fails
/// this test on the way in.
#[test]
fn the_real_fixtures_name_every_record_no_parser_arm_reads() {
    materialize_tree();

    let desktop = parse_claude_code_jsonl(
        &ctx(&format!(
            "{BASE}/claude-desktop/ac0e34b8-76ab-4d62-bd0c-c67ed97bf5c0.jsonl"
        ))
        .with_agent_label(Some("claude_desktop".to_string())),
    )
    .unwrap();
    assert_eq!(
        desktop.skipped_kinds,
        [
            ("ai-title".to_string(), 5),
            ("attachment".to_string(), 4),
            ("last-prompt".to_string(), 3),
            ("mode".to_string(), 1),
        ]
        .into_iter()
        .collect(),
        "Claude Desktop's unmodelled record types, by its own name for each"
    );
    // `queue-operation` appears 4 times in the same file and is deliberately
    // absent: it is modelled and declined, which is a decision, not a failure.
    assert!(!desktop.skipped_kinds.contains_key("queue-operation"));
    // Only the records that DATE themselves become events — an undated record
    // cannot be placed in a conversation, so it reports through the tally alone.
    let unknown_events: Vec<&str> = desktop
        .events
        .iter()
        .filter(|e| e.kind != "user_message" && e.kind != "assistant_message")
        .map(|e| e.kind.as_str())
        .collect();
    assert_eq!(unknown_events, ["attachment"; 4]);
    assert!(
        desktop
            .events
            .iter()
            .filter(|e| e.kind == "attachment")
            .all(|e| e.content_excerpt.is_none() && e.content_bytes.is_none()),
        "an unknown shape ships the fact it existed and none of what it said"
    );

    let codex = parse_codex_rollout(&ctx(&format!(
        "{BASE}/codex/rollout-2026-08-05T13-58-57-019fd1ca-816d-7af2-9332-a6db0bfc4d25.jsonl"
    )))
    .unwrap();
    assert_eq!(
        codex.skipped_kinds,
        [
            ("event_msg/task_complete".to_string(), 1),
            ("event_msg/task_started".to_string(), 1),
        ]
        .into_iter()
        .collect(),
        "codex's tally qualifies by envelope; the EVENT carries the bare kind"
    );
    // `response_item/message` and `/reasoning` duplicate the `event_msg` rows
    // this parser already reads — declined on purpose, so never counted.
    assert!(!codex.skipped_kinds.contains_key("response_item/message"));

    // The three parsers whose fixtures this build understands completely.
    for (label, res) in [
        (
            "claude",
            parse_claude_code_jsonl(&ctx(&format!(
                "{BASE}/claude/11111111-1111-1111-1111-111111111111.jsonl"
            )))
            .unwrap(),
        ),
        (
            "pi",
            parse_pi_session(&ctx_api(&format!(
                "{BASE}/pi/2026-06-26T23-53-00-262Z_019f0659-dc65-7969-af42-5dc1ced6232a.jsonl"
            )))
            .unwrap(),
        ),
        (
            "cursor",
            parse_cursor_tracking_db(&ctx(&format!("{BASE}/cursor/state.vscdb"))).unwrap(),
        ),
    ] {
        assert!(
            res.skipped_kinds.is_empty(),
            "{label}: every record in this fixture has an arm — got {:?}",
            res.skipped_kinds
        );
    }
}

/// Assert a parser's streaming mode yields the same events (via the sink), tool
/// calls, and stats as its collect mode.
fn assert_stream_matches<F>(collected: modelstat_parsers::ParseResult, stream: F, label: &str)
where
    F: FnOnce(&mut dyn FnMut(Vec<RawEvent>)) -> modelstat_parsers::ParseResult,
{
    let mut streamed_events: Vec<RawEvent> = Vec::new();
    let res = stream(&mut |chunk| streamed_events.extend(chunk));
    assert!(
        res.events.is_empty(),
        "{label}: streaming mode must not accumulate events"
    );
    assert_eq!(
        streamed_events, collected.events,
        "{label}: streamed events differ"
    );
    assert_eq!(
        res.tool_calls, collected.tool_calls,
        "{label}: streamed tool_calls differ"
    );
    assert_eq!(res.stats, collected.stats, "{label}: streamed stats differ");
}

/// Regenerate the frozen goldens from CURRENT parser output:
/// `REGEN_GOLDENS=1 cargo test -p modelstat-parsers --test golden_parsers regen`.
/// No-op without the env var. The goldens exist so parser-output drift is a
/// deliberate, reviewed diff — regenerate, then READ the diff before committing.
#[test]
fn regen_goldens() {
    if std::env::var("REGEN_GOLDENS").is_err() {
        return;
    }
    materialize_tree();
    let dump = |name: &str, res: &modelstat_parsers::types::ParseResult| {
        let path = format!("{}/{name}", golden_dir());
        let prev = golden(name);
        let sc: Vec<Value> = res
            .script_contexts
            .iter()
            .map(|c| {
                serde_json::json!({
                    "external_call_id": c.external_call_id,
                    "command": c.command,
                    "cwd": c.cwd,
                })
            })
            .collect();
        let v = serde_json::json!({
            "deviceId": prev.get("deviceId").cloned().unwrap_or_else(|| Value::from("dev_1")),
            "sourceFile": res.source_file,
            "events": res.events,
            "toolCalls": res.tool_calls,
            "scriptContexts": sc,
            "stats": res.stats,
        });
        let mut text = serde_json::to_string_pretty(&v).unwrap();
        text.push('\n');
        std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {path}: {e}"));
    };
    let claude = |sid: &str| format!("{BASE}/claude/{sid}.jsonl");
    dump(
        "claude_basic.json",
        &parse_claude_code_jsonl(&ctx(&claude("11111111-1111-1111-1111-111111111111"))).unwrap(),
    );
    dump(
        "claude_synthetic.json",
        &parse_claude_code_jsonl(&ctx(&claude("22222222-2222-2222-2222-222222222222"))).unwrap(),
    );
    dump(
        "claude_resume_copy.json",
        &parse_claude_code_jsonl(&ctx(&format!(
            "{BASE}/claude-resume/projects/myproj/44444444-4444-4444-4444-444444444444.jsonl"
        )))
        .unwrap(),
    );
    dump(
        "claude_desktop.json",
        &parse_claude_code_jsonl(
            &ctx(&format!(
                "{BASE}/claude-desktop/ac0e34b8-76ab-4d62-bd0c-c67ed97bf5c0.jsonl"
            ))
            .with_agent_label(Some("claude_desktop".to_string())),
        )
        .unwrap(),
    );
    dump(
        "codex_basic.json",
        &parse_codex_rollout(&ctx(&format!(
            "{BASE}/codex/rollout-2026-08-05T13-58-57-019fd1ca-816d-7af2-9332-a6db0bfc4d25.jsonl"
        )))
        .unwrap(),
    );
    dump(
        "pi_basic.json",
        &parse_pi_session(&ctx_api(&format!(
            "{BASE}/pi/2026-06-26T23-53-00-262Z_019f0659-dc65-7969-af42-5dc1ced6232a.jsonl"
        )))
        .unwrap(),
    );
    dump(
        "cursor_basic.json",
        &parse_cursor_tracking_db(&ctx(&format!("{BASE}/cursor/state.vscdb"))).unwrap(),
    );
}

/// Time parity, asserted on the REAL fixtures rather than on a shape: the two
/// fields every derived wait is computed from must mean the same thing for
/// every agent.
///
///   * `ts` — the instant the SOURCE stated, on every event. An event with an
///     empty or unparseable instant does not merely lack a field: it parses as
///     epoch 0 and lands in 1970, dragging the session's whole timeline with it.
///   * `turn_index` — the ordinal of the TYPED PROMPT the event belongs to, for
///     all four parsers. Codex used to count API round trips here instead, so
///     this single-prompt rollout reported turns 0, 1 and 2.
#[test]
fn every_real_fixture_dates_every_event_and_numbers_turns_by_the_prompt() {
    materialize_tree();

    let codex_file = format!(
        "{BASE}/codex/rollout-2026-08-05T13-58-57-019fd1ca-816d-7af2-9332-a6db0bfc4d25.jsonl"
    );
    let desktop_file = format!("{BASE}/claude-desktop/ac0e34b8-76ab-4d62-bd0c-c67ed97bf5c0.jsonl");
    let pi_file =
        format!("{BASE}/pi/2026-06-26T23-53-00-262Z_019f0659-dc65-7969-af42-5dc1ced6232a.jsonl");

    let codex = parse_codex_rollout(&ctx(&codex_file)).unwrap();
    let desktop = parse_claude_code_jsonl(
        &ctx(&desktop_file).with_agent_label(Some("claude_desktop".to_string())),
    )
    .unwrap();
    let pi = parse_pi_session(&ctx_api(&pi_file)).unwrap();
    let cursor = parse_cursor_tracking_db(&ctx(&format!("{BASE}/cursor/state.vscdb"))).unwrap();

    for (label, res) in [
        ("codex", &codex),
        ("claude_desktop", &desktop),
        ("pi", &pi),
        ("cursor", &cursor),
    ] {
        for e in &res.events {
            assert!(
                e.ts.len() >= 19 && e.ts.contains('T') && e.ts.starts_with("20"),
                "{label}: event {} carries no stated instant ({:?})",
                e.source_event_id,
                e.ts
            );
            assert!(
                e.turn_index.is_some(),
                "{label}: event {} states no turn",
                e.source_event_id
            );
        }
        for c in &res.tool_calls {
            assert!(
                c.started_at.len() >= 19 && c.started_at.contains('T'),
                "{label}: tool call {} starts at no stated instant",
                c.external_call_id
            );
        }
    }

    // Codex: ONE typed prompt in this rollout, so one turn — including the
    // records the parser models no arm for.
    assert!(
        codex.events.iter().all(|e| e.turn_index == Some(0)),
        "one prompt is one turn: {:?}",
        codex
            .events
            .iter()
            .map(|e| (e.kind.as_str(), e.turn_index))
            .collect::<Vec<_>>()
    );
    // The duration codex measured for that turn — its own number, which nothing
    // downstream can derive from the timestamps.
    assert_eq!(
        codex
            .events
            .iter()
            .find(|e| e.kind == "task_complete")
            .and_then(|e| e.duration_ms),
        Some(6556),
        "codex's stated turn duration reaches the wire"
    );

    // Claude Desktop: two typed prompts in this transcript, and the ordinal
    // moves only at them — tool-result-only user lines inherit the turn.
    assert_eq!(
        desktop.events.iter().filter_map(|e| e.turn_index).max(),
        Some(1),
        "two prompts, turns 0 and 1"
    );
}
