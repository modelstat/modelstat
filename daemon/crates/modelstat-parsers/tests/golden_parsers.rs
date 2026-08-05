//! Parser golden parity — §4.4. Reconstructs the exact `/tmp/modelstat-fixtures`
//! tree the TS generator (`daemon/scripts/fixtures/gen-parsers.mts`) wrote, runs
//! the Rust parsers against the SAME canonical paths, and asserts byte-for-byte
//! equal RawEvent / ToolCallDraft output against the frozen goldens in
//! `modelstat-wire/tests/golden/parsers/`.
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

/// Recreate `/tmp/modelstat-fixtures` byte-for-byte from the committed inputs.
fn materialize_tree() {
    let src_root = tree_dir();
    let _ = std::fs::remove_dir_all(BASE);
    copy_tree(Path::new(&src_root), Path::new(BASE));
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
/// Stated explicitly because the parsers no longer decide this: they stamp
/// whatever the scan resolved from the machine's own auth material. The
/// fixtures encode a subscription login, so the context must say so — a test
/// that left it at the `unknown` floor would be pinning the wrong contract.
fn ctx(source_file: &str) -> ParserContext {
    ParserContext::new("dev_1", source_file)
        .with_pricing_mode(modelstat_parsers::auth_mode::PRICING_MODE_SUBSCRIPTION)
}

/// Same, for the pi fixtures — pi has no subscription path, it bills a key.
fn ctx_api(source_file: &str) -> ParserContext {
    ParserContext::new("dev_1", source_file)
        .with_pricing_mode(modelstat_parsers::auth_mode::PRICING_MODE_API)
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
