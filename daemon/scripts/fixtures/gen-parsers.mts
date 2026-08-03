/**
 * Golden fixtures — §4.4 (parser fixtures). Real transcript samples run through
 * the actual TS parsers; the emitted RawEvent/ToolCallDraft JSON is frozen.
 *
 * Determinism/portability: transcripts are written under a FIXED base path
 * (`/tmp/modelstat-fixtures`, identical on macOS + the Linux CI runner where the
 * generator runs) so `source_file` and the path-derived `source_event_id`s are
 * stable byte-for-byte across machines. The M2 Rust parsers, given the same
 * canonical paths, must reproduce these outputs.
 */
import { DatabaseSync } from "node:sqlite";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { parseClaudeCodeJsonl } from "../../../packages/parsers/src/claude-code/index.js";
import { parseCodexRollout } from "../../../packages/parsers/src/codex/index.js";
import { parsePiSession } from "../../../packages/parsers/src/pi/index.js";
import { parseCursorTrackingDb } from "../../../packages/parsers/src/cursor/index.js";
import { type Generator, writeGolden } from "./lib.mts";

const BASE = "/tmp/modelstat-fixtures";

function writeLines(path: string, lines: object[]): string {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, lines.map((l) => JSON.stringify(l)).join("\n"));
  return path;
}

const CLAUDE_SID = "11111111-1111-1111-1111-111111111111";
const CWD = "/Users/dev/projects/myrepo";

function claudeAssistant(uuid: string, model: string, blocks: object[]): object {
  return {
    type: "assistant",
    uuid,
    sessionId: CLAUDE_SID,
    timestamp: "2026-06-01T10:00:05.000Z",
    cwd: CWD,
    message: {
      role: "assistant",
      model,
      content: blocks,
      usage: { input_tokens: 10, output_tokens: 20, cache_creation_input_tokens: 0, cache_read_input_tokens: 5 },
    },
  };
}
function claudeUser(uuid: string, content: unknown): object {
  return { type: "user", uuid, sessionId: CLAUDE_SID, timestamp: "2026-06-01T10:00:00.000Z", cwd: CWD, message: { role: "user", content } };
}

async function claudeFixtures(): Promise<void> {
  // 1. Basic: user → assistant(text+tool_use) → tool_result.
  const basic = writeLines(join(BASE, "claude", `${CLAUDE_SID}.jsonl`), [
    claudeUser("u1", "add a retry to the uploader"),
    claudeAssistant("a1", "claude-opus-4-7", [
      { type: "text", text: "On it." },
      { type: "tool_use", id: "toolu_1", name: "Bash", input: { command: "npm test" } },
    ]),
    claudeUser("u2", [{ type: "tool_result", tool_use_id: "toolu_1", content: "ok" }]),
  ]);
  const basicRes = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: basic, pricingMode: "subscription" });
  writeGolden("parsers/claude_basic.json", { deviceId: "dev_1", sourceFile: basic, ...basicRes });

  // 2. <synthetic> kept verbatim; does not contaminate lastModel.
  const synthetic = writeLines(join(BASE, "claude", "22222222-2222-2222-2222-222222222222.jsonl"), [
    claudeAssistant("s1", "claude-opus-4-7", [{ type: "text", text: "real reply" }]),
    claudeAssistant("s2", "<synthetic>", [{ type: "text", text: "Prompt is too long" }]),
    claudeUser("s3", "next prompt"),
  ]);
  const synthRes = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: synthetic, pricingMode: "subscription" });
  writeGolden("parsers/claude_synthetic.json", {
    deviceId: "dev_1",
    sourceFile: synthetic,
    models: synthRes.events.map((e) => e.model),
    events: synthRes.events,
  });

  // 3. Resume-copy dedupe (orphaned-ancestor case §4.4): a line whose sessionId
  //    ≠ the filename uuid is a copy, dropped when the ancestor <sid>.jsonl
  //    exists under the projects root.
  const ancestorSid = "33333333-3333-3333-3333-333333333333";
  const copySid = "44444444-4444-4444-4444-444444444444";
  const projRoot = join(BASE, "claude-resume", "projects", "myproj");
  // Ancestor file must exist under the root so the copy is recognized.
  writeLines(join(projRoot, `${ancestorSid}.jsonl`), [
    { type: "user", uuid: "anc-u1", sessionId: ancestorSid, timestamp: "2026-06-01T09:00:00.000Z", cwd: CWD, message: { role: "user", content: "original" } },
  ]);
  const copyFile = writeLines(join(projRoot, `${copySid}.jsonl`), [
    // Copy of an ancestor line (sessionId = ancestor ≠ filename) → dropped.
    { type: "user", uuid: "anc-u1", sessionId: ancestorSid, timestamp: "2026-06-01T09:00:00.000Z", cwd: CWD, message: { role: "user", content: "original" } },
    // Native line (sessionId = filename) → kept.
    { type: "user", uuid: "copy-u1", sessionId: copySid, timestamp: "2026-06-01T11:00:00.000Z", cwd: CWD, message: { role: "user", content: "new work in resumed session" } },
  ]);
  const resumeRes = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: copyFile, pricingMode: "subscription" });
  writeGolden("parsers/claude_resume_copy.json", {
    deviceId: "dev_1",
    sourceFile: copyFile,
    note: "the ancestor-sessioned copy line is dropped; only the native line survives",
    events: resumeRes.events,
  });
}

const CODEX_SID = "55555555-5555-5555-5555-555555555555";
async function codexFixtures(): Promise<void> {
  const lines = [
    { timestamp: "2026-06-08T15:49:00.000Z", type: "session_meta", payload: { id: CODEX_SID, cwd: "/Users/dev/projects/api" } },
    { timestamp: "2026-06-08T15:49:01.000Z", type: "turn_context", payload: { cwd: "/Users/dev/projects/api", model: "gpt-5.5" } },
    { timestamp: "2026-06-08T15:49:39.854Z", type: "response_item", payload: { type: "function_call", name: "update_plan", arguments: JSON.stringify({ plan: [{ step: "do it", status: "pending" }] }), call_id: "call_1" } },
    { timestamp: "2026-06-08T15:49:40.072Z", type: "response_item", payload: { type: "function_call_output", call_id: "call_1", output: "Plan updated" } },
    // Disjoint token buckets (the double-billing fix §7.1): input−cached,
    // output−reasoning, cache_read=cached, reasoning=reasoning. Counters live at
    // payload.info.last_token_usage — the per-call delta. `total_token_usage` is
    // the cumulative session total and is deliberately DIFFERENT here so a parser
    // that reads the wrong one fails this fixture instead of silently summing
    // cumulative counters.
    { timestamp: "2026-06-08T15:50:00.000Z", type: "event_msg", payload: { type: "token_count", info: { total_token_usage: { input_tokens: 900, cached_input_tokens: 300, cache_write_input_tokens: 0, output_tokens: 400, reasoning_output_tokens: 150, total_tokens: 1300 }, last_token_usage: { input_tokens: 100, cached_input_tokens: 30, cache_write_input_tokens: 0, output_tokens: 50, reasoning_output_tokens: 20, total_tokens: 150 }, model_context_window: 272000 }, rate_limits: null } },
    // Rate-limits-only token_count (`info` absent): no usage to record, so this
    // emits NO event rather than a phantom zero-token turn.
    { timestamp: "2026-06-08T15:50:01.000Z", type: "event_msg", payload: { type: "token_count", rate_limits: { primary_used_percent: 12.5 } } },
  ];
  const file = writeLines(join(BASE, "codex", `rollout-2026-06-08T15-49-00-${CODEX_SID}.jsonl`), lines);
  const res = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file, pricingMode: "subscription" });
  writeGolden("parsers/codex_basic.json", { deviceId: "dev_1", sourceFile: file, events: res.events, toolCalls: res.toolCalls });
}

const PI_SID = "019f0659-dc65-7969-af42-5dc1ced6232a";
async function piFixtures(): Promise<void> {
  const lines = [
    { type: "session", version: 3, id: PI_SID, timestamp: "2026-06-26T23:53:00.262Z", cwd: "/Users/dev/projects/acme/myrepo" },
    { type: "model_change", id: "0e64cd33", parentId: null, timestamp: "2026-06-26T23:53:39.971Z", provider: "anthropic", modelId: "claude-opus-4-8" },
    { type: "message", id: "229ad794", parentId: "571f4bbe", timestamp: "2026-06-26T23:53:39.971Z", message: { role: "user", content: [{ type: "text", text: "fix the bug please" }], timestamp: 1782518019963 } },
    {
      type: "message", id: "d2888ba8", parentId: "229ad794", timestamp: "2026-06-26T23:53:44.372Z",
      message: {
        role: "assistant",
        content: [{ type: "text", text: "Let me look." }, { type: "toolCall", id: "tc_pi_1", name: "read_file", arguments: { path: "src/x.ts" } }],
        api: "anthropic-messages", provider: "anthropic", model: "claude-opus-4-8",
        usage: { input: 2, output: 142, cacheRead: 10, cacheWrite: 26300, totalTokens: 26444 }, stopReason: "toolUse",
      },
    },
    { type: "message", id: "43c1351d", parentId: "d2888ba8", timestamp: "2026-06-26T23:53:44.382Z", message: { role: "toolResult", toolCallId: "tc_pi_1", toolName: "read_file", content: [{ type: "text", text: "a\nb\nc" }], isError: false } },
  ];
  const file = writeLines(join(BASE, "pi", `2026-06-26T23-53-00-262Z_${PI_SID}.jsonl`), lines);
  const res = await parsePiSession({ deviceId: "dev_1", sourceFile: file, pricingMode: "api" });
  writeGolden("parsers/pi_basic.json", { deviceId: "dev_1", sourceFile: file, events: res.events, toolCalls: res.toolCalls });
}

async function cursorFixtures(): Promise<void> {
  // Build a Cursor tracking DB (ai_code_hashes) with node's built-in SQLite
  // (a standard SQLite file the parser's sql.js reads back). The parser is
  // dormant behind MODELSTAT_ENABLE_CURSOR_PARSER (§7.1); the fixture still
  // freezes its output shape for the M2 port.
  const file = join(BASE, "cursor", "state.vscdb");
  mkdirSync(dirname(file), { recursive: true });
  rmSync(file, { force: true });
  const db = new DatabaseSync(file);
  db.exec(
    `CREATE TABLE ai_code_hashes (hash TEXT, source TEXT, model TEXT, requestId TEXT, conversationId TEXT, timestamp INTEGER);
     INSERT INTO ai_code_hashes VALUES ('h1','composer','claude-opus-4-8','req_1','conv_1',1782518000000);
     INSERT INTO ai_code_hashes VALUES ('h2','composer','claude-opus-4-8','req_2','conv_1',1782518060000);`,
  );
  db.close();
  const res = await parseCursorTrackingDb({ deviceId: "dev_1", sourceFile: file });
  writeGolden("parsers/cursor_basic.json", { deviceId: "dev_1", sourceFile: file, events: res.events, toolCalls: res.toolCalls });
}

export const generator: Generator = {
  category: "parsers (§4.4)",
  run: async () => {
    rmSync(BASE, { recursive: true, force: true });
    await claudeFixtures();
    await codexFixtures();
    await piFixtures();
    await cursorFixtures();
  },
};
