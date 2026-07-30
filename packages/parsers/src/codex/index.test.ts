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

/** A `token_count` line in codex's real shape: counters under `info.last_token_usage`. */
function tokenCount(
  timestamp = "2026-06-08T15:50:00.000Z",
  last: Record<string, number> = {
    input_tokens: 100,
    cached_input_tokens: 0,
    cache_write_input_tokens: 0,
    output_tokens: 50,
    reasoning_output_tokens: 0,
    total_tokens: 150,
  },
): object {
  return {
    timestamp,
    type: "event_msg",
    payload: {
      type: "token_count",
      info: {
        // Deliberately different from `last` so a parser reading the cumulative
        // total instead of the per-call delta fails loudly here.
        total_token_usage: {
          input_tokens: 9000,
          cached_input_tokens: 0,
          cache_write_input_tokens: 0,
          output_tokens: 4000,
          reasoning_output_tokens: 0,
          total_tokens: 13000,
        },
        last_token_usage: last,
        model_context_window: 272000,
      },
      rate_limits: null,
    },
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
  assert.equal(call.action?.surface, "builtin", "non-shell tool → builtin surface");
  assert.equal(call.action?.executable, "update_plan");
  assert.ok(call.source_event_id.startsWith("evt_"), "source_event_id from line offset");
});

test("exec_command normalises to the shell wire name; action filled later", async () => {
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
  // The on-device extractor fills structural facts (surface/executable + a
  // redacted command); semantics stay server-side.
  assert.equal(call.action?.surface, "shell");
  assert.equal(call.action?.executable, "git");
  assert.equal(call.status, "success");
});

test("local_shell_call normalises to the shell wire name; action filled later", async () => {
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
  // The extractor takes the leading program as executable (no bash -lc
  // unwrapping at parse time — semantics are server-side).
  assert.equal(call.action?.surface, "shell");
  assert.equal(call.action?.executable, "bash");
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

test("token_count buckets are stored DISJOINT (no reasoning/cache double-count)", async () => {
  // OpenAI reports input_tokens INCLUSIVE of cached, output_tokens INCLUSIVE of
  // reasoning. The parser must store them disjoint so the pricer (which bills
  // input + cache_read + output + reasoning as separate line items) doesn't pay
  // for cached and reasoning tokens twice (G7/G8).
  const file = writeRollout([
    sessionMeta(),
    turnContext("o3"),
    tokenCount("2026-06-08T15:50:00.000Z", {
      input_tokens: 100, // INCLUSIVE of the 40 cached
      cached_input_tokens: 40,
      cache_write_input_tokens: 0,
      output_tokens: 1000, // INCLUSIVE of the 600 reasoning
      reasoning_output_tokens: 600,
      total_tokens: 1100,
    }),
  ]);
  const { events } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  const e = events.find((x) => x.tokens);
  assert.ok(e?.tokens, "token_count emits an event carrying tokens");
  const t = e.tokens;
  assert.equal(t.input, 60, "input excludes the 40 cached");
  assert.equal(t.output, 400, "output excludes the 600 reasoning");
  assert.equal(t.cache_read, 40);
  assert.equal(t.reasoning, 600);
  // Total is preserved — the disjoint buckets re-add to OpenAI's inclusive totals.
  assert.equal(t.input + t.cache_read, 100, "input + cache_read == input_tokens");
  assert.equal(t.output + t.reasoning, 1000, "output + reasoning == output_tokens");
});

test("token counters are read from info.last_token_usage, not the cumulative total", async () => {
  // The regression that made EVERY codex event land with 0 tokens: the parser read
  // `payload.input_tokens`, but codex nests counters under
  // `payload.info.last_token_usage`. `tokenCount`'s `total_token_usage` is set to
  // a different, much larger value, so reading the wrong one is visible here.
  const file = writeRollout([sessionMeta(), turnContext("gpt-5.6-sol"), tokenCount()]);
  const { events } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  const e = events.find((x) => x.tokens);
  assert.ok(e?.tokens, "a nested token_count still emits a usage event");
  assert.equal(e.model, "gpt-5.6-sol");
  assert.equal(e.tokens.input, 100, "per-call delta, not the 9000 cumulative total");
  assert.equal(e.tokens.output, 50, "per-call delta, not the 4000 cumulative total");
});

test("a rate-limits-only token_count emits NO event (no phantom zero-token turn)", async () => {
  const file = writeRollout([
    sessionMeta(),
    turnContext("gpt-5.6-sol"),
    {
      timestamp: "2026-06-08T15:50:00.000Z",
      type: "event_msg",
      payload: { type: "token_count", rate_limits: { primary_used_percent: 12.5 } },
    },
  ]);
  const { events } = await parseCodexRollout({ deviceId: "dev_1", sourceFile: file });
  assert.equal(
    events.filter((e) => e.tokens).length,
    0,
    "no usage in the line → no usage event, rather than one reporting 0 tokens",
  );
});

test("a token_count whose counters moved THROWS instead of recording zeros", async () => {
  // Fail loud on upstream schema drift: silently zeroing under-reports real spend.
  const file = writeRollout([
    sessionMeta(),
    turnContext("gpt-5.6-sol"),
    {
      timestamp: "2026-06-08T15:50:00.000Z",
      type: "event_msg",
      payload: {
        type: "token_count",
        info: { last_token_usage: { prompt_tokens: 100, completion_tokens: 50 } },
      },
    },
  ]);
  await assert.rejects(
    () => parseCodexRollout({ deviceId: "dev_1", sourceFile: file }),
    /schema drift/,
    "an unreadable counter must stop the parse, not default to 0",
  );
});
