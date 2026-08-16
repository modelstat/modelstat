import { strict as assert } from "node:assert";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { deriveSessionIdFromPiPath, parsePiSession } from "./index.js";

const SESSION = "019f0659-dc65-7969-af42-5dc1ced6232a";
const TS_USER = "2026-06-26T23:53:39.971Z";
const TS_ASSISTANT = "2026-06-26T23:53:44.372Z";
const TS_RESULT = "2026-06-26T23:53:44.382Z";

function writeSession(lines: object[]): string {
  const dir = mkdtempSync(join(tmpdir(), "modelstat-pi-"));
  const file = join(dir, `2026-06-26T23-53-00-262Z_${SESSION}.jsonl`);
  writeFileSync(file, lines.map((l) => JSON.stringify(l)).join("\n"));
  return file;
}

function sessionLine(): object {
  return {
    type: "session",
    version: 3,
    id: SESSION,
    timestamp: "2026-06-26T23:53:00.262Z",
    cwd: "/Users/dev/projects/acme/myrepo",
  };
}

function modelChange(provider = "anthropic", modelId = "claude-opus-4-8"): object {
  return {
    type: "model_change",
    id: "0e64cd33",
    parentId: null,
    timestamp: TS_USER,
    provider,
    modelId,
  };
}

function userMessage(text: string): object {
  return {
    type: "message",
    id: "229ad794",
    parentId: "571f4bbe",
    timestamp: TS_USER,
    message: { role: "user", content: [{ type: "text", text }], timestamp: 1782518019963 },
  };
}

function assistantMessage(
  opts: { toolCall?: { id: string; name: string; arguments: unknown } } = {},
): object {
  const content: object[] = [{ type: "text", text: "Let me look." }];
  if (opts.toolCall) {
    content.push({ type: "toolCall", ...opts.toolCall });
  }
  return {
    type: "message",
    id: "d2888ba8",
    parentId: "229ad794",
    timestamp: TS_ASSISTANT,
    message: {
      role: "assistant",
      content,
      api: "anthropic-messages",
      provider: "anthropic",
      model: "claude-opus-4-8",
      usage: { input: 2, output: 142, cacheRead: 10, cacheWrite: 26300, totalTokens: 26444 },
      stopReason: "toolUse",
    },
  };
}

function toolResult(toolCallId: string, toolName: string, isError = false): object {
  return {
    type: "message",
    id: "43c1351d",
    parentId: "d2888ba8",
    timestamp: TS_RESULT,
    message: {
      role: "toolResult",
      toolCallId,
      toolName,
      content: [{ type: "text", text: "a\nb\nc" }],
      isError,
    },
  };
}

test("derives session id from pi filename", () => {
  assert.equal(deriveSessionIdFromPiPath(`/x/2026-06-26T23-53-00-262Z_${SESSION}.jsonl`), SESSION);
  assert.equal(deriveSessionIdFromPiPath("/x/not-a-session.jsonl"), null);
});

test("emits user + assistant events with mapped tokens", async () => {
  const file = writeSession([
    sessionLine(),
    modelChange(),
    userMessage("fix the bug please"),
    assistantMessage(),
  ]);
  const { events } = await parsePiSession({ deviceId: "dev_1", sourceFile: file });
  assert.equal(events.length, 2);

  const user = events[0]!;
  assert.equal(user.kind, "user_message");
  assert.equal(user.agent, "pi");
  assert.equal(user.session_id, SESSION);
  assert.equal(user.provider, "anthropic");
  assert.equal(user.tokens, null);
  assert.equal(user.content_excerpt, "fix the bug please");

  const asst = events[1]!;
  assert.equal(asst.kind, "assistant_message");
  assert.equal(asst.provider, "anthropic");
  assert.equal(asst.model, "claude-opus-4-8");
  assert.deepEqual(asst.tokens, {
    input: 2,
    output: 142,
    cache_creation: 26300,
    cache_read: 10,
    reasoning: 0,
  });
  assert.equal(asst.cwd, "/Users/dev/projects/acme/myrepo");
  assert.equal(asst.git?.remote_slug, "acme/myrepo");
  // The slug is a path-shape guess: it says so, and it names no forge.
  assert.equal(asst.git?.slug_source, "path_shape");
  assert.equal(asst.git?.remote_host, null);
});

test("pairs toolCall with its toolResult and aggregates", async () => {
  const file = writeSession([
    sessionLine(),
    modelChange(),
    userMessage("list files"),
    assistantMessage({ toolCall: { id: "toolu_1", name: "ls", arguments: { path: "." } } }),
    toolResult("toolu_1", "ls"),
  ]);
  const { events, toolCalls } = await parsePiSession({ deviceId: "dev_1", sourceFile: file });

  assert.equal(toolCalls.length, 1);
  const call = toolCalls[0]!;
  assert.equal(call.agent, "pi");
  assert.equal(call.server, "builtin");
  assert.equal(call.name, "ls");
  assert.equal(call.external_call_id, "toolu_1");
  assert.equal(call.status, "success");
  assert.equal(call.started_at, TS_ASSISTANT);
  assert.equal(call.ended_at, TS_RESULT);
  assert.ok(call.result_bytes > 0, "result bytes recorded");
  assert.ok(call.args_bytes > 0, "args bytes recorded");

  const asst = events.find((e) => e.kind === "assistant_message")!;
  assert.deepEqual(asst.tool_calls, { ls: 1 });
});

test("marks failed tool results as error", async () => {
  const file = writeSession([
    sessionLine(),
    modelChange(),
    assistantMessage({
      toolCall: { id: "toolu_2", name: "Bash", arguments: { command: "false" } },
    }),
    toolResult("toolu_2", "Bash", true),
  ]);
  const { toolCalls } = await parsePiSession({ deviceId: "dev_1", sourceFile: file });
  assert.equal(toolCalls[0]!.status, "error");
});

test("maps non-anthropic providers", async () => {
  const file = writeSession([
    sessionLine(),
    {
      type: "model_change",
      id: "a",
      parentId: null,
      timestamp: TS_USER,
      provider: "openai",
      modelId: "gpt-5",
    },
    {
      type: "message",
      id: "x",
      parentId: null,
      timestamp: TS_ASSISTANT,
      message: {
        role: "assistant",
        content: [{ type: "text", text: "hi" }],
        provider: "openai",
        model: "gpt-5",
        usage: { input: 1, output: 1 },
      },
    },
  ]);
  const { events } = await parsePiSession({ deviceId: "dev_1", sourceFile: file });
  assert.equal(events[0]!.provider, "openai");
});
