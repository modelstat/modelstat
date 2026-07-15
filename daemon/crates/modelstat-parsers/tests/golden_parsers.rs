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

fn ctx(source_file: &str) -> ParserContext {
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
        let want_sc: Vec<ScriptCtx> =
            serde_json::from_value(g["scriptContexts"].clone()).unwrap();
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
        let file =
            format!("{BASE}/claude-resume/projects/myproj/44444444-4444-4444-4444-444444444444.jsonl");
        let res = parse_claude_code_jsonl(&ctx(&file)).unwrap();
        let g = golden("claude_resume_copy.json");
        assert_eq!(res.events, events_of(&g), "claude_resume_copy events");
    }

    // 4. Codex — disjoint token buckets, tool call anchored to its own line.
    {
        let file = format!(
            "{BASE}/codex/rollout-2026-06-08T15-49-00-55555555-5555-5555-5555-555555555555.jsonl"
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
        let res = parse_pi_session(&ctx(&file)).unwrap();
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
}
