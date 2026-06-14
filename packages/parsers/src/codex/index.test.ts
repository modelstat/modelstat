import { strict as assert } from "node:assert";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { parseCodexRollout } from "./index.js";

const SESSION = "11111111-1111-1111-1111-111111111111";
const TS_CALL = "2026-06-08T15:49:39.854Z";
const TS_OUT = "2026-06-08T15:49:40.072Z";

function writeRollout(lines: object[]): string {
  const dir = mkdtempSync(join(tmpdir(), "modelstat-codex-"));
  const file = join(dir, `rollout-2026-06-08T15-49-00-${SESSION}.jsonl`);
  writeFileSync(file, lines.map((l) => JSON.stringify(l)).join("\n"));
  return file;
}

function sessionMeta(): object {
  return {
    timestamp: "2026-06-08T15:49:00.000Z",
    type: "session_meta",
    payload: { id: SESSION, cwd: "/Users/dev/projects/myrepo" },
  };
}

function turnContext(model = "gpt-5.5"): object {
  return {
    timestamp: "2026-06-08T15:49:01.000Z",
    type: "turn_context",
    payload: { cwd: "/Users/dev/projects/myrepo", model },
  };
}

function responseItem(payload: object, timestamp = TS_CALL): object {
  return { timestamp, type: "response_item", payload };
}

function functionCall(callId: string, name: string, args: object): object {
  return responseItem({
    type: "function_call",
    name,
    arguments: JSON.stringify(args),
    call_id: callId,
  });
}

function functionCallOutput(callId: string, output: string): object {
  return responseItem({ type: "function_call_output", call_id: callId, output }, TS_OUT);
}

function tokenCount(timestamp = "2026-06-08T15:50:00.000Z"): object {
  return {
    timestamp,
    type: "event_msg",
    payload: { type: "token_count", input_tokens: 100, output_tokens: 50 },
  };
}

test("pairs function_call with its output by call_id", async () => {
  const file = writeRollout([
    sessionMeta(),
    turnContext(),
    functionCall("call_1", "update_plan", { plan: [{ step: "do it", status: "pending" }] }),
    functionCallOutput("call_1", "Plan updated"),
  ]);
  const { toolCalls } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  assert.equal(toolCalls.length, 1, "one draft per call");
  const call = toolCalls[0];
  assert.ok(call, "draft emitted");
  assert.equal(call.external_call_id, "call_1");
  assert.equal(call.session_id, SESSION);
  assert.equal(call.agent, "codex_cli");
  assert.equal(call.server, "builtin");
  assert.equal(call.name, "update_plan", "non-shell tools keep their name");
  assert.equal(call.status, "success", "matched output → success");
  assert.equal(call.started_at, TS_CALL, "started_at = ts of the call line");
  assert.equal(call.ended_at, TS_OUT, "ended_at = ts of the output line");
  assert.equal(call.model, "gpt-5.5", "model from turn_context payload");
  assert.equal(call.turn_index, 0);
  assert.equal(call.call_index, 0);
  assert.ok(call.args_hash.length === 64, "args hashed");
  assert.notEqual(call.signature_hash, "none", "object args have a signature");
  assert.ok(call.args_bytes > 0);
  assert.ok(call.result_bytes > 0, "output bytes recorded");
  assert.deepEqual(call.command_families, [], "no families for non-shell tools");
  assert.ok(call.source_event_id.startsWith("evt_"), "source_event_id from line offset");
});

test("exec_command maps to shell with command families from the cmd string", async () => {
  const file = writeRollout([
    sessionMeta(),
    turnContext(),
    functionCall("call_sh", "exec_command", {
      cmd: "git status && cargo build | tee /tmp/out.log",
      workdir: "/Users/dev/projects/myrepo",
    }),
    functionCallOutput("call_sh", "Process exited with code 0"),
  ]);
  const { toolCalls } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  const call = toolCalls[0];
  assert.ok(call, "draft emitted");
  assert.equal(call.name, "shell", "shell-ish calls are normalised to shell");
  assert.equal(call.server, "builtin");
  assert.deepEqual(call.command_families, ["git", "cargo"], "allowlisted verbs only");
  assert.equal(call.status, "success");
});

test("local_shell_call unwraps the bash -lc argv wrapper for families", async () => {
  const file = writeRollout([
    sessionMeta(),
    turnContext(),
    responseItem({
      type: "local_shell_call",
      call_id: "call_lsc",
      status: "completed",
      action: { type: "exec", command: ["bash", "-lc", "npm test && npx tsc && playwright test"] },
    }),
    responseItem({ type: "local_shell_call_output", call_id: "call_lsc", output: "ok" }, TS_OUT),
  ]);
  const { toolCalls } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  const call = toolCalls[0];
  assert.ok(call, "draft emitted");
  assert.equal(call.name, "shell");
  assert.equal(call.server, "builtin");
  assert.deepEqual(call.command_families, ["npm", "npx", "playwright"]);
  assert.equal(call.status, "success");
  assert.equal(call.ended_at, TS_OUT);
});

test("mcp_tool_call maps server/name into the mcp:<server> form", async () => {
  const file = writeRollout([
    sessionMeta(),
    turnContext(),
    responseItem({
      type: "mcp_tool_call",
      call_id: "call_mcp",
      server: "github",
      tool: "create_pr",
      arguments: JSON.stringify({ title: "hello" }),
    }),
    responseItem({ type: "mcp_tool_call_output", call_id: "call_mcp", output: "done" }, TS_OUT),
    tokenCount(),
  ]);
  const { events, toolCalls } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  const call = toolCalls[0];
  assert.ok(call, "draft emitted");
  assert.equal(call.server, "mcp:github");
  assert.equal(call.name, "create_pr");
  assert.equal(call.status, "success");
  const assistant = events.find((e) => e.kind === "assistant_message");
  assert.ok(assistant, "assistant event emitted");
  assert.deepEqual(
    assistant.tool_calls,
    { "mcp:github/create_pr": 1 },
    "aggregate keyed by canonical identity",
  );
});

test("aggregate map attaches to the next assistant event, then resets", async () => {
  const file = writeRollout([
    sessionMeta(),
    turnContext(),
    functionCall("call_a", "exec_command", { cmd: "git log" }),
    functionCallOutput("call_a", "..."),
    functionCall("call_b", "exec_command", { cmd: "ls -la" }),
    functionCallOutput("call_b", "..."),
    functionCall("call_c", "update_plan", { plan: [] }),
    tokenCount("2026-06-08T15:50:00.000Z"),
    tokenCount("2026-06-08T15:51:00.000Z"),
  ]);
  const { events, toolCalls } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  assert.equal(toolCalls.length, 3);
  const assistants = events.filter((e) => e.kind === "assistant_message");
  assert.equal(assistants.length, 2);
  assert.deepEqual(
    assistants[0]?.tool_calls,
    { shell: 2, update_plan: 1 },
    "calls since the last assistant event attach to the next one",
  );
  assert.deepEqual(assistants[1]?.tool_calls, {}, "aggregate resets after attaching");
});

test("drafts are still emitted when no assistant event ever follows", async () => {
  const file = writeRollout([
    sessionMeta(),
    turnContext(),
    functionCall("call_only", "exec_command", { cmd: "git status" }),
    functionCallOutput("call_only", "clean"),
  ]);
  const { events, toolCalls } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  assert.equal(events.length, 0, "no assistant event in the file");
  assert.equal(toolCalls.length, 1, "per-call drafts ship regardless");
  assert.equal(toolCalls[0]?.status, "success");
});

test("a tool-call line with no ts and no prior lastTs is skipped (no wall-clock started_at)", async () => {
  // No session_meta / turn_context (those carry the only earlier
  // timestamps), and the response_item itself omits `timestamp`. With
  // neither r.timestamp nor a prior lastTs available, the draft must be
  // skipped rather than stamped with the current wall clock — a
  // wall-clock started_at would make the same event look fresh on every
  // re-parse and break the idempotent-replay invariant.
  const file = writeRollout([
    { type: "session_meta", payload: { id: SESSION, cwd: "/Users/dev/projects/myrepo" } },
    {
      type: "response_item",
      payload: {
        type: "function_call",
        name: "update_plan",
        arguments: JSON.stringify({ plan: [] }),
        call_id: "call_no_ts",
      },
    },
  ]);
  const { toolCalls, stats } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  assert.equal(toolCalls.length, 0, "ts-less call with no prior ts emits no draft");
  assert.ok(stats.skipped >= 1, "the ts-less call line is counted as skipped");
});

test("ts-less tool-call inherits the last line ts deterministically (no wall clock)", async () => {
  // session_meta carries a timestamp; the tool-call line itself does not.
  // started_at must fall back to that last-seen ts (deterministic), never
  // the wall clock — re-parsing the same file yields the identical value.
  const metaTs = "2026-06-08T15:49:00.000Z";
  const file = writeRollout([
    { timestamp: metaTs, type: "session_meta", payload: { id: SESSION, cwd: "/r" } },
    {
      type: "response_item",
      payload: {
        type: "function_call",
        name: "update_plan",
        arguments: JSON.stringify({ plan: [] }),
        call_id: "call_inherit",
      },
    },
  ]);
  const { toolCalls } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  assert.equal(toolCalls.length, 1, "draft emitted via the inherited ts");
  assert.equal(
    toolCalls[0]?.started_at,
    metaTs,
    "started_at = last-seen line ts, deterministic across re-parses",
  );
});

test("id-only local_shell_call pairs with its id-only output (no call_id on either side)", async () => {
  // The call carries only `id` (no call_id); its output likewise keys on
  // `id`. The call side stores under firstString(call_id, id) = id, so the
  // output lookup must mirror that derivation or the pair never matches.
  const file = writeRollout([
    sessionMeta(),
    turnContext(),
    responseItem({
      type: "local_shell_call",
      id: "shell_only_id",
      status: "completed",
      action: { type: "exec", command: ["bash", "-lc", "git status"] },
    }),
    responseItem(
      { type: "local_shell_call_output", id: "shell_only_id", output: "On branch main" },
      TS_OUT,
    ),
  ]);
  const { toolCalls } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  assert.equal(toolCalls.length, 1, "one draft for the id-only shell call");
  const call = toolCalls[0];
  assert.ok(call, "draft emitted");
  assert.equal(call.external_call_id, "shell_only_id", "external id falls back to `id`");
  assert.equal(call.name, "shell");
  assert.equal(call.status, "success", "id-only output pairs → status derived");
  assert.equal(call.ended_at, TS_OUT, "ended_at set from the output line");
  assert.ok(call.result_bytes > 0, "result bytes recorded from the paired output");
});

test("unmatched call stays status unknown with no ended_at", async () => {
  const file = writeRollout([
    sessionMeta(),
    turnContext(),
    functionCall("call_lost", "exec_command", { cmd: "git push" }),
  ]);
  const { toolCalls } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  const call = toolCalls[0];
  assert.ok(call, "draft emitted");
  assert.equal(call.status, "unknown", "no output line → unknown");
  assert.equal(call.ended_at, null);
  assert.equal(call.result_bytes, 0);
});
