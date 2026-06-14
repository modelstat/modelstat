import { describe, expect, it } from "vitest";
import type { AdapterConfig } from "@modelstat/adapters-protocol";
import { processFrame } from "../src/interpreter/network.js";

const chatgptAdapter: AdapterConfig = {
  protocol_version: 1,
  provider: "chatgpt_web",
  vendor: "openai",
  adapter_version: 1,
  match: ["https://chatgpt.com/*"],
  default_model: "gpt-5",
  extractors: {
    conversation_id: [
      {
        kind: "network.responseJsonPath",
        urlPattern: ".*backend-api/conversation.*",
        sse: true,
        path: "$.conversation_id",
      },
    ],
    model: [
      {
        kind: "network.responseJsonPath",
        urlPattern: ".*backend-api/conversation.*",
        sse: true,
        path: "$.message.metadata.model_slug",
      },
    ],
    messages: [
      {
        kind: "network.responseJsonPath",
        urlPattern: ".*backend-api/conversation.*",
        sse: true,
        messageIdPath: "$.message.id",
        rolePath: "$.message.author.role",
        textPath: "$.message.content.parts[*]",
        usagePath: "$.message.metadata.finish_details.usage",
      },
    ],
  },
  tokenizer: { default: "tiktoken/o200k_base" },
  invariants: [],
};

describe("network interpreter", () => {
  it("parses a multi-frame SSE completion and merges text + usage", () => {
    const reqId = "r1";
    const host = "chatgpt.com";
    const href = "https://chatgpt.com/c/abc";

    processFrame(
      {
        type: "request",
        id: reqId,
        url: "https://chatgpt.com/backend-api/conversation",
        method: "POST",
        requestBody: null,
        startedAt: 1,
      },
      chatgptAdapter,
      host,
      href,
    );

    processFrame(
      {
        type: "response_start",
        id: reqId,
        status: 200,
        contentType: "text/event-stream",
      },
      chatgptAdapter,
      host,
      href,
    );

    const chunk1 = [
      `data: ${JSON.stringify({
        conversation_id: "abc",
        message: {
          id: "m1",
          author: { role: "assistant" },
          content: { parts: ["Hello "] },
          metadata: { model_slug: "gpt-5" },
        },
      })}`,
      "",
      "",
    ].join("\n");
    const chunk2 = [
      `data: ${JSON.stringify({
        message: {
          id: "m1",
          author: { role: "assistant" },
          content: { parts: ["world."] },
          metadata: {
            model_slug: "gpt-5",
            finish_details: { usage: { prompt_tokens: 12, completion_tokens: 34 } },
          },
        },
      })}`,
      "",
      "",
    ].join("\n");

    const out1 = processFrame(
      { type: "response_chunk", id: reqId, chunks: [chunk1] },
      chatgptAdapter,
      host,
      href,
    );
    const out2 = processFrame(
      { type: "response_chunk", id: reqId, chunks: [chunk2] },
      chatgptAdapter,
      host,
      href,
    );

    const allMsgs = [...out1.messages, ...out2.messages];
    const m1 = allMsgs.filter((m) => m.messageId === "m1");
    // At least one message emission in each chunk.
    expect(allMsgs.length).toBeGreaterThanOrEqual(1);
    // Final chunk should have carried the usage.
    const withUsage = m1.find((m) => m.usage.input !== null);
    expect(withUsage?.usage.input).toBe(12);
    expect(withUsage?.usage.output).toBe(34);
  });

  it("ignores non-matching url patterns", () => {
    const out = processFrame(
      {
        type: "request",
        id: "r2",
        url: "https://chatgpt.com/static/asset.js",
        method: "GET",
        requestBody: null,
        startedAt: 1,
      },
      chatgptAdapter,
      "chatgpt.com",
      "https://chatgpt.com/",
    );
    expect(out.messages).toEqual([]);
  });
});
