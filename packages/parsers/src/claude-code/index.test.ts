import { strict as assert } from "node:assert";
import { createHash, randomUUID } from "node:crypto";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { sourceEventId } from "@modelstat/core";
import { parseClaudeCodeJsonl } from "./index.js";

function writeTranscript(lines: object[]): string {
  const dir = mkdtempSync(join(tmpdir(), "modelstat-claude-code-"));
  const file = join(dir, "transcript.jsonl");
  writeFileSync(file, lines.map((l) => JSON.stringify(l)).join("\n"));
  return file;
}

const SESSION = "11111111-1111-1111-1111-111111111111";

function assistantLineIn(sessionId: string, uuid: string, model: string, text: string): object {
  return {
    type: "assistant",
    uuid,
    sessionId,
    timestamp: "2026-06-01T10:00:00.000Z",
    cwd: "/Users/dev/projects/myrepo",
    message: {
      role: "assistant",
      model,
      content: [{ type: "text", text }],
      usage: { input_tokens: 10, output_tokens: 20 },
    },
  };
}

function userLineIn(sessionId: string, uuid: string, text: string): object {
  return {
    type: "user",
    uuid,
    sessionId,
    timestamp: "2026-06-01T10:00:01.000Z",
    cwd: "/Users/dev/projects/myrepo",
    message: { role: "user", content: text },
  };
}

function assistantLine(uuid: string, model: string, text: string): object {
  return assistantLineIn(SESSION, uuid, model, text);
}

function userLine(uuid: string, text: string): object {
  return userLineIn(SESSION, uuid, text);
}

test("keeps <synthetic> verbatim on its own event", async () => {
  const file = writeTranscript([
    assistantLine("a1", "claude-opus-4-7", "real reply"),
    assistantLine("a2", "<synthetic>", "Prompt is too long"),
  ]);
  const { events } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const models = events.map((e) => e.model);
  assert.deepEqual(models, ["claude-opus-4-7", "<synthetic>"]);
});

test("<synthetic> does not contaminate lastModel for following user messages", async () => {
  const file = writeTranscript([
    assistantLine("a1", "claude-opus-4-7", "real reply"),
    assistantLine("a2", "<synthetic>", "No response requested."),
    userLine("u1", "next prompt"),
  ]);
  const { events } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const user = events.find((e) => e.kind === "user_message");
  assert.ok(user, "user event emitted");
  assert.equal(user.model, "claude-opus-4-7", "user message attributed to the real model");
});

test("user message before any real assistant reply has null model", async () => {
  const file = writeTranscript([
    assistantLine("a1", "<synthetic>", "API Error: Request timed out"),
    userLine("u1", "first prompt"),
  ]);
  const { events } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const user = events.find((e) => e.kind === "user_message");
  assert.ok(user, "user event emitted");
  assert.equal(user.model, null, "no real model seen yet");
});

// ── tool_call extraction ────────────────────────────────────────────────

function assistantToolLine(
  uuid: string,
  model: string,
  blocks: object[],
  ts = "2026-06-01T10:00:00.000Z",
): object {
  return {
    type: "assistant",
    uuid,
    sessionId: SESSION,
    timestamp: ts,
    cwd: "/Users/dev/projects/myrepo",
    message: {
      role: "assistant",
      model,
      content: blocks,
      usage: { input_tokens: 10, output_tokens: 20 },
    },
  };
}

function toolUseBlock(id: string | null, name: string, input?: unknown): object {
  return {
    type: "tool_use",
    ...(id ? { id } : {}),
    name,
    ...(input === undefined ? {} : { input }),
  };
}

function toolResultLine(uuid: string, results: object[], ts = "2026-06-01T10:00:02.000Z"): object {
  return {
    type: "user",
    uuid,
    sessionId: SESSION,
    timestamp: ts,
    cwd: "/Users/dev/projects/myrepo",
    message: { role: "user", content: results },
  };
}

function toolResultBlock(toolUseId: string, content: unknown, isError = false): object {
  return {
    type: "tool_result",
    tool_use_id: toolUseId,
    content,
    ...(isError ? { is_error: true } : {}),
  };
}

test("multi-call assistant line: one draft per tool_use block, no extra events", async () => {
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [
      { type: "text", text: "let me look" },
      toolUseBlock("toolu_01", "Bash", { command: "git status" }),
      toolUseBlock("toolu_02", "Read", { file_path: "/tmp/x.ts" }),
    ]),
  ]);
  const { events, toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });

  assert.equal(events.length, 1, "tool calls never become RawEvents");
  assert.equal(events[0]?.kind, "assistant_message");
  assert.equal(toolCalls.length, 2);

  const [bash, read] = toolCalls;
  assert.ok(bash && read);
  assert.equal(bash.external_call_id, "toolu_01");
  assert.equal(bash.name, "Bash");
  assert.equal(bash.server, "builtin");
  assert.equal(bash.call_index, 0);
  assert.equal(read.external_call_id, "toolu_02");
  assert.equal(read.name, "Read");
  assert.equal(read.call_index, 1);
  for (const c of toolCalls) {
    assert.equal(c.agent, "claude_code");
    assert.equal(c.session_id, SESSION);
    assert.equal(
      c.source_event_id,
      events[0]?.source_event_id,
      "drafts anchor on the assistant event",
    );
    assert.equal(c.started_at, "2026-06-01T10:00:00.000Z");
    assert.equal(c.turn_index, null, "parser tracks no turn index");
    assert.equal(c.model, "claude-opus-4-7");
  }
});

test("aggregate tool_calls map on the assistant event counts per identity", async () => {
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [
      toolUseBlock("toolu_01", "Bash", { command: "ls" }),
      toolUseBlock("toolu_02", "Bash", { command: "cat README.md" }),
      toolUseBlock("toolu_03", "mcp__github__create_pr", { title: "hi" }),
    ]),
  ]);
  const { events } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  assert.deepEqual(events[0]?.tool_calls, { Bash: 2, "mcp:github/create_pr": 1 });
});

test("tool_result in a later user line completes the call", async () => {
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [
      toolUseBlock("toolu_01", "Bash", { command: "git status" }),
    ]),
    toolResultLine("u1", [toolResultBlock("toolu_01", "clean tree")], "2026-06-01T10:00:05.000Z"),
  ]);
  const { events, toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });

  assert.equal(events.length, 2, "assistant + user events, nothing extra");
  const call = toolCalls[0];
  assert.ok(call);
  assert.equal(call.status, "success");
  assert.equal(call.ended_at, "2026-06-01T10:00:05.000Z");
  assert.equal(call.result_bytes, Buffer.byteLength(JSON.stringify("clean tree"), "utf8"));
});

test("is_error tool_result marks the call as error", async () => {
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [
      toolUseBlock("toolu_01", "Bash", { command: "false" }),
    ]),
    toolResultLine("u1", [toolResultBlock("toolu_01", [{ type: "text", text: "exit 1" }], true)]),
  ]);
  const { toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const call = toolCalls[0];
  assert.ok(call);
  assert.equal(call.status, "error");
  assert.equal(
    call.result_bytes,
    Buffer.byteLength(JSON.stringify([{ type: "text", text: "exit 1" }]), "utf8"),
  );
});

test("unmatched tool_use stays unknown with null ended_at", async () => {
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [
      toolUseBlock("toolu_01", "WebSearch", { query: "hello" }),
    ]),
  ]);
  const { toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const call = toolCalls[0];
  assert.ok(call);
  assert.equal(call.status, "unknown");
  assert.equal(call.ended_at, null);
  assert.equal(call.result_bytes, 0);
});

test("MCP tool names map to mcp:<server>/<tool>", async () => {
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [
      toolUseBlock("toolu_01", "mcp__github__create_pr", { title: "x" }),
    ]),
  ]);
  const { toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const call = toolCalls[0];
  assert.ok(call);
  assert.equal(call.server, "mcp:github");
  assert.equal(call.name, "create_pr");
});

test("dynamic hex tails in tool names collapse to <dyn>", async () => {
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [
      toolUseBlock("toolu_01", "mcp__linear__subscribe_a1b2c3d4e5f6", {}),
    ]),
  ]);
  const { events, toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const call = toolCalls[0];
  assert.ok(call);
  assert.equal(call.server, "mcp:linear");
  assert.equal(call.name, "subscribe_<dyn>");
  assert.deepEqual(events[0]?.tool_calls, { "mcp:linear/subscribe_<dyn>": 1 });
});

test("tool calls get a structural action on-device; raw command is reduced to facts", async () => {
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [
      toolUseBlock("toolu_01", "Bash", { command: "git status && pnpm -r test | jq .ok" }),
      toolUseBlock("toolu_02", "Read", { file_path: "/tmp/x.ts" }),
      toolUseBlock("toolu_03", "Bash", { command: "./run-my-secret-script.sh --now" }),
    ]),
  ]);
  const { toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  // The on-device extractor fills `action` with STRUCTURAL facts + a redacted
  // command only; the whole operation frame is server-derived (SPEC 0004).
  assert.equal(toolCalls[0]?.action?.surface, "shell");
  assert.equal(toolCalls[0]?.action?.executable, "git");
  assert.equal(toolCalls[1]?.action?.surface, "builtin");
  assert.equal(toolCalls[1]?.action?.executable, "Read");
  assert.equal(toolCalls[2]?.action?.surface, "shell");
  assert.equal(toolCalls[2]?.action?.executable, "run-my-secret-script.sh");
  // The raw command is still reduced to a hash; only structural facts + the
  // redacted command ride the draft.
  assert.equal(toolCalls[0]?.args_hash.length, 64);
  assert.ok(toolCalls[0]?.action?.command_redacted?.includes("git"));
});

test("hashes are stable hex; signature from sorted key names; empty input handled", async () => {
  const input = { command: "git status" };
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [
      toolUseBlock("toolu_01", "Bash", input),
      toolUseBlock("toolu_02", "Bash", input),
      toolUseBlock("toolu_03", "ExitPlanMode"),
    ]),
  ]);
  const { toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const [first, second, noInput] = toolCalls;
  assert.ok(first && second && noInput);

  assert.match(first.args_hash, /^[0-9a-f]{64}$/, "args_hash is hex sha256");
  assert.equal(first.args_hash, second.args_hash, "same input → same hash");
  assert.equal(first.args_hash, createHash("sha256").update(JSON.stringify(input)).digest("hex"));
  assert.equal(first.signature_hash, createHash("sha256").update("command").digest("hex"));
  assert.equal(first.args_bytes, Buffer.byteLength(JSON.stringify(input), "utf8"));

  assert.equal(noInput.args_hash, "", "no input → empty args_hash");
  assert.equal(noInput.signature_hash, "none");
  assert.equal(noInput.args_bytes, 0);
});

test("the shipped command is redacted: secrets stripped, raw verbatim never ships", async () => {
  // A real-format secret (caught by redact's floor) inside a shell command, the
  // Write content, and the tool result.
  const secret = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789";
  const command = `curl -H 'Authorization: Bearer ${secret}' https://internal.example.com/deploy`;
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [
      toolUseBlock("toolu_01", "Bash", { command }),
      toolUseBlock("toolu_02", "Write", { file_path: "/Users/dev/.ssh/id_rsa", content: `token=${secret}` }),
    ]),
    toolResultLine("u1", [toolResultBlock("toolu_01", `deployed, ${secret}`)]),
  ]);
  const { toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const shipped = JSON.stringify(toolCalls);
  // The secret never appears — not from the command, the Write content, or the result.
  assert.ok(!shipped.includes(secret), "secrets are redacted off the wire");
  // The raw command verbatim never ships — only its redacted form, with the
  // secret replaced by a redaction marker.
  assert.ok(!shipped.includes(command), "raw command verbatim never ships");
  assert.ok(shipped.includes("[REDACTED:"), "the secret was replaced by a redaction marker");
  // Non-secret infra IS allowed to ship — it's the org's own data going to the
  // org's own analytics.
  assert.ok(shipped.includes("internal.example.com"), "non-secret infra ships");
  // Non-shell tools (Write) carry no command_redacted; their raw path/content
  // are reduced to hashes and never echoed onto the draft.
  assert.ok(!shipped.includes("id_rsa"), "non-shell tool inputs aren't echoed");
});

test("tool calls on a <synthetic> assistant line keep the model verbatim", async () => {
  const file = writeTranscript([
    assistantLine("a1", "claude-opus-4-7", "real reply"),
    assistantToolLine("a2", "<synthetic>", [toolUseBlock("toolu_01", "Bash", { command: "ls" })]),
    userLine("u1", "next prompt"),
  ]);
  const { events, toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  assert.equal(
    toolCalls[0]?.model,
    "<synthetic>",
    "draft keeps the issuing message's model verbatim",
  );
  const user = events.find((e) => e.kind === "user_message");
  assert.equal(user?.model, "claude-opus-4-7", "lastModel still uncontaminated");
});

test("missing tool_use id falls back to a deterministic tc_ id", async () => {
  const file = writeTranscript([
    assistantToolLine("a1", "claude-opus-4-7", [toolUseBlock(null, "Bash", { command: "ls" })]),
  ]);
  const first = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const second = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const id = first.toolCalls[0]?.external_call_id;
  assert.ok(id?.startsWith("tc_"), `fallback id has tc_ prefix, got ${id}`);
  assert.equal(id, second.toolCalls[0]?.external_call_id, "stable across re-parses");
});

test("top-level tool_use line yields a draft but no RawEvent", async () => {
  const file = writeTranscript([
    assistantLine("a1", "claude-opus-4-7", "real reply"),
    {
      type: "tool_use",
      id: "toolu_top1",
      name: "Bash",
      input: { command: "git status" },
      timestamp: "2026-06-01T10:00:03.000Z",
      sessionId: SESSION,
    },
    toolResultLine("u1", [toolResultBlock("toolu_top1", "clean")], "2026-06-01T10:00:04.000Z"),
  ]);
  const { events, toolCalls } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });

  assert.deepEqual(
    events.map((e) => e.kind),
    ["assistant_message", "user_message"],
    "the tool_use line emits no event",
  );
  assert.equal(toolCalls.length, 1);
  const call = toolCalls[0];
  assert.ok(call);
  assert.equal(call.external_call_id, "toolu_top1");
  assert.equal(call.started_at, "2026-06-01T10:00:03.000Z");
  assert.equal(call.model, "claude-opus-4-7", "attributed to the session's last real model");
  assert.equal(call.status, "success", "paired with the later tool_result");
  assert.equal(call.ended_at, "2026-06-01T10:00:04.000Z");
  assert.ok(call.action, "structural action extracted on-device");
  assert.ok(call.source_event_id, "anchored on its own line offset");
  assert.notEqual(call.source_event_id, events[0]?.source_event_id);
});

// ── Resume-copy dedupe ──────────────────────────────────────────────
// `claude --resume` writes a new <new-session-uuid>.jsonl that begins
// with byte-identical copies of the ancestor session's lines (original
// sessionId + uuid preserved), then the new session's own lines. These
// tests pin the policy: copies are dropped while the ancestor's file
// exists, or emitted once under a position-independent uuid key when it
// doesn't — so no (session, uuid) pair ever yields two surviving dedupe
// keys across parses. Session ids are random per run so the on-disk
// ancestor probe can never hit leftovers from earlier runs.

/** Path is `<dir>/<sessionId>.jsonl` — the layout the resume detection
 * keys on (line sessionId vs filename uuid). */
function writeSessionFile(dir: string, sessionId: string, lines: object[]): string {
  const file = join(dir, `${sessionId}.jsonl`);
  writeFileSync(file, lines.map((l) => JSON.stringify(l)).join("\n"));
  return file;
}

/** Non-event preamble line; resumed files start with these. Each one
 * shifts the byte offsets of everything below it, which is exactly what
 * breaks (file, byteOffset) dedupe for the copied lines. */
function summaryLine(): object {
  return { type: "summary", summary: "previous conversation", leafUuid: randomUUID() };
}

function originalSession(sid: string): object[] {
  return [
    userLineIn(sid, randomUUID(), "first prompt"),
    assistantLineIn(sid, randomUUID(), "claude-opus-4-7", "first reply"),
  ];
}

test("resume copies are skipped while the ancestor file exists", async () => {
  const dir = mkdtempSync(join(tmpdir(), "modelstat-claude-code-"));
  const sidA = randomUUID();
  const sidB = randomUUID();
  const linesA = originalSession(sidA);
  const fileA = writeSessionFile(dir, sidA, linesA);
  const fileB = writeSessionFile(dir, sidB, [
    summaryLine(),
    ...linesA,
    userLineIn(sidB, randomUUID(), "resumed prompt"),
    assistantLineIn(sidB, randomUUID(), "claude-opus-4-7", "resumed reply"),
  ]);

  const a = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: fileA });
  const b = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: fileB });

  // The resumed file contributes only its own session's lines…
  assert.deepEqual(
    b.events.map((e) => e.session_id),
    [sidB, sidB],
    "copied ancestor lines must not be re-emitted",
  );
  // …so the ancestor session's events survive exactly once across both
  // parses, and every dedupe key is unique.
  const all = [...a.events, ...b.events];
  assert.equal(all.filter((e) => e.session_id === sidA).length, 2);
  assert.equal(new Set(all.map((e) => e.source_event_id)).size, all.length);
});

test("chained resume copies are skipped across sibling project dirs", async () => {
  // Resumes routinely hop project dirs (one worktree per session → a
  // different encoded dir per cwd), so the ancestor probe must look
  // beyond the resumed file's own dir.
  const root = mkdtempSync(join(tmpdir(), "modelstat-claude-code-"));
  const dirA = join(root, "-Users-dev-proj--worktrees-a");
  const dirB = join(root, "-Users-dev-proj--worktrees-b");
  const dirC = join(root, "-Users-dev-proj--worktrees-c");
  for (const d of [dirA, dirB, dirC]) mkdirSync(d);
  const sidA = randomUUID();
  const sidB = randomUUID();
  const sidC = randomUUID();
  const linesA = originalSession(sidA);
  const newLinesB = [
    userLineIn(sidB, randomUUID(), "second leg prompt"),
    assistantLineIn(sidB, randomUUID(), "claude-opus-4-7", "second leg reply"),
  ];
  writeSessionFile(dirA, sidA, linesA);
  writeSessionFile(dirB, sidB, [summaryLine(), ...linesA, ...newLinesB]);
  // Third leg copies BOTH ancestors' lines (real files carry 2–4
  // distinct sessionIds).
  const fileC = writeSessionFile(dirC, sidC, [
    summaryLine(),
    summaryLine(),
    ...linesA,
    ...newLinesB,
    userLineIn(sidC, randomUUID(), "third leg prompt"),
  ]);

  const c = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: fileC });
  assert.deepEqual(
    c.events.map((e) => e.session_id),
    [sidC],
    "both ancestors' copies skipped, own lines kept",
  );
});

test("orphaned copies (ancestor file pruned) are emitted once under a position-independent key", async () => {
  const root = mkdtempSync(join(tmpdir(), "modelstat-claude-code-"));
  const dirB = join(root, "proj-b");
  const dirC = join(root, "proj-c");
  for (const d of [dirB, dirC]) mkdirSync(d);
  const sidA = randomUUID(); // ancestor session; its file exists nowhere
  const sidB = randomUUID();
  const sidC = randomUUID();
  const linesA = originalSession(sidA);
  // Two independent resumes of the pruned session — same copied lines
  // at different byte offsets in different files.
  const fileB = writeSessionFile(dirB, sidB, [
    summaryLine(),
    ...linesA,
    userLineIn(sidB, randomUUID(), "resume one"),
  ]);
  const fileC = writeSessionFile(dirC, sidC, [
    summaryLine(),
    summaryLine(),
    ...linesA,
    userLineIn(sidC, randomUUID(), "resume two"),
  ]);

  const b = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: fileB });
  const c = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: fileC });

  // History preserved: copies emitted under the ancestor session, usage intact.
  const aFromB = b.events.filter((e) => e.session_id === sidA);
  const aFromC = c.events.filter((e) => e.session_id === sidA);
  assert.equal(aFromB.length, 2);
  assert.ok(
    aFromB.some((e) => e.kind === "assistant_message" && e.tokens?.input === 10),
    "copied usage preserved",
  );
  // The copies really do sit at different offsets…
  assert.notDeepEqual(
    aFromB.map((e) => e.source_byte_offset),
    aFromC.map((e) => e.source_byte_offset),
  );
  // …yet produce identical dedupe keys, so the server keeps exactly one
  // of each (session, uuid) pair…
  assert.deepEqual(
    aFromB.map((e) => e.source_event_id),
    aFromC.map((e) => e.source_event_id),
  );
  // …while each file's own new lines keep distinct keys.
  const own = [...b.events, ...c.events].filter((e) => e.session_id !== sidA);
  assert.equal(own.length, 2);
  assert.equal(new Set(own.map((e) => e.source_event_id)).size, own.length);
});

test("non-copied lines keep the historical (file, byteOffset) dedupe key", async () => {
  // Regression guard: re-keying normal lines would make the server
  // re-ingest all previously-uploaded history as duplicates.
  const dir = mkdtempSync(join(tmpdir(), "modelstat-claude-code-"));
  const sidA = randomUUID();
  const fileA = writeSessionFile(dir, sidA, originalSession(sidA));
  const { events } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: fileA });
  assert.equal(events.length, 2);
  for (const e of events) {
    assert.ok(typeof e.source_byte_offset === "number", "offset recorded");
    assert.equal(e.source_event_id, sourceEventId("dev_1", fileA, e.source_byte_offset));
  }
});

test("model attribution still flows from skipped copies into resumed lines", async () => {
  const dir = mkdtempSync(join(tmpdir(), "modelstat-claude-code-"));
  const sidA = randomUUID();
  const sidB = randomUUID();
  const linesA = originalSession(sidA);
  writeSessionFile(dir, sidA, linesA);
  // First new line after the copies is a user message — its model must
  // come from the copied assistant reply even though that copy's event
  // was dropped.
  const fileB = writeSessionFile(dir, sidB, [
    summaryLine(),
    ...linesA,
    userLineIn(sidB, randomUUID(), "resumed prompt"),
  ]);

  const { events } = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: fileB });
  assert.equal(events.length, 1);
  assert.equal(events[0]!.kind, "user_message");
  assert.equal(events[0]!.model, "claude-opus-4-7");
});

// ── tool_use → aggregate tool_calls counts ───────────────────────────

function assistantLineWithBlocks(uuid: string, blocks: unknown[]): object {
  return {
    type: "assistant",
    uuid,
    sessionId: SESSION,
    timestamp: "2026-06-01T10:00:02.000Z",
    cwd: "/Users/dev/projects/myrepo",
    message: {
      role: "assistant",
      model: "claude-fable-5",
      content: blocks,
      usage: { input_tokens: 12, output_tokens: 34, cache_read_input_tokens: 500 },
    },
  };
}

test("assistant tool_use blocks become aggregate tool_calls counts", async () => {
  const file = writeTranscript([
    userLine("u1", "please fix the build"),
    assistantLineWithBlocks("a1", [
      { type: "text", text: "On it." },
      { type: "tool_use", name: "Bash", input: { command: "npm test" } },
      { type: "tool_use", name: "Bash", input: { command: "npm run build" } },
      { type: "tool_use", name: "Read", input: { file_path: "/x" } },
      { type: "tool_use" }, // nameless — ignored, never counted as ""
    ]),
  ]);
  const res = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  const a = res.events.find((e) => e.kind === "assistant_message");
  assert.ok(a);
  assert.deepEqual(a.tool_calls, { Bash: 2, Read: 1 });
  assert.equal(a.tokens?.cache_read, 500);
  // Tool arguments must never leak into the event.
  assert.ok(!JSON.stringify(a.tool_calls).includes("npm test"));
});

test("user messages and text-only assistants carry empty tool_calls", async () => {
  const file = writeTranscript([userLine("u1", "hello"), assistantLine("a1", "m", "hi")]);
  const res = await parseClaudeCodeJsonl({ deviceId: "dev_1", sourceFile: file });
  assert.equal(res.events.length, 2);
  for (const e of res.events) {
    assert.deepEqual(e.tool_calls, {});
  }
});
