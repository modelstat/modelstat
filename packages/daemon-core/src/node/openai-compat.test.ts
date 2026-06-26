/**
 * Wire-level tests for the remote OpenAI-compatible adapters.
 *
 * Each test spins a real `node:http` server speaking the
 * `/v1/chat/completions` contract and points the adapter at it — a fake
 * at the wire, not a mocked SDK — so we exercise the actual request the
 * daemon would send and the actual parsing of the reply.
 */

import assert from "node:assert/strict";
import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { test } from "node:test";
import {
  defaultOpenAICompatConfig,
  type OpenAICompatConfig,
  OpenAICompatConfigError,
  OpenAICompatRequestError,
  openaiCognize,
  openaiSummarize,
} from "./openai-compat.js";

interface CapturedRequest {
  authorization: string | undefined;
  body: {
    model?: string;
    messages?: Array<{ role: string; content: string }>;
    max_tokens?: number;
    max_completion_tokens?: number;
    temperature?: number;
  };
}

interface FakeEndpoint {
  readonly cfg: OpenAICompatConfig;
  readonly captured: CapturedRequest[];
  close: () => Promise<void>;
}

interface FakeReply {
  status: number;
  json?: unknown;
  /** Raw response body. When set, sent verbatim instead of `JSON.stringify(json)`. */
  raw?: string;
  /** Extra response headers (e.g. `retry-after`). */
  headers?: Record<string, string>;
}

/** Start a fake chat-completions endpoint. `reply` decides the HTTP
 * status + body per request; the request is recorded for asserts. */
async function startEndpoint(reply: () => FakeReply): Promise<FakeEndpoint> {
  const captured: CapturedRequest[] = [];
  const server: Server = createServer((req: IncomingMessage, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const raw = Buffer.concat(chunks).toString("utf8");
      captured.push({
        authorization: req.headers.authorization,
        body: raw ? JSON.parse(raw) : {},
      });
      const r = reply();
      res.writeHead(r.status, { "content-type": "application/json", ...(r.headers ?? {}) });
      res.end(r.raw ?? JSON.stringify(r.json));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as AddressInfo).port;
  return {
    cfg: {
      baseUrl: `http://127.0.0.1:${port}/v1`,
      apiKey: "sk-test",
      model: "test-model",
      timeoutMs: 2_000,
    },
    captured,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

function chatReply(content: string): FakeReply {
  return { status: 200, json: { choices: [{ message: { content } }] } };
}

test("openaiSummarize: posts the summariser prompt and returns the trimmed answer", async () => {
  const ep = await startEndpoint(() => chatReply("Fixed a null deref in the auth middleware."));
  try {
    const summarize = openaiSummarize(ep.cfg);
    const out = await summarize({ prompt: "Summarise this session.", maxTokens: 64 });

    assert.equal(out, "Fixed a null deref in the auth middleware.");
    const req = ep.captured[0];
    assert.ok(req, "endpoint received a request");
    assert.equal(req.authorization, "Bearer sk-test");
    assert.equal(req.body.model, "test-model");
    assert.equal(req.body.messages?.[0]?.role, "system");
    assert.equal(req.body.messages?.[1]?.content, "Summarise this session.");
  } finally {
    await ep.close();
  }
});

test("chatComplete: standard model sends max_tokens + temperature", async () => {
  const ep = await startEndpoint(() => chatReply("ok"));
  try {
    await openaiSummarize(ep.cfg)({ prompt: "p", maxTokens: 64 });
    const req = ep.captured[0];
    assert.ok(req);
    assert.equal(typeof req.body.max_tokens, "number");
    assert.equal(typeof req.body.temperature, "number");
    assert.equal(req.body.max_completion_tokens, undefined);
  } finally {
    await ep.close();
  }
});

test("chatComplete: reasoning model sends max_completion_tokens and omits temperature", async () => {
  const ep = await startEndpoint(() => chatReply("ok"));
  try {
    const cfg = { ...ep.cfg, model: "o3-mini" };
    await openaiSummarize(cfg)({ prompt: "p", maxTokens: 64 });
    const req = ep.captured[0];
    assert.ok(req);
    assert.equal(typeof req.body.max_completion_tokens, "number");
    assert.equal(req.body.max_tokens, undefined);
    assert.equal(req.body.temperature, undefined);
  } finally {
    await ep.close();
  }
});

test("chatComplete: retries a transient 503 then succeeds", async () => {
  let calls = 0;
  const ep = await startEndpoint(() => {
    calls += 1;
    return calls === 1 ? { status: 503, json: { error: "warming up" } } : chatReply("recovered");
  });
  try {
    const out = await openaiSummarize(ep.cfg)({ prompt: "p", maxTokens: 64 });
    assert.equal(out, "recovered");
    assert.equal(calls, 2, "first attempt 503, second attempt 200");
  } finally {
    await ep.close();
  }
});

test("chatComplete: honours a 429 Retry-After header then succeeds", async () => {
  let calls = 0;
  const captured: number[] = [];
  const ep = await startEndpoint(() => {
    calls += 1;
    captured.push(Date.now());
    if (calls === 1) return { status: 429, json: {}, headers: { "retry-after": "0" } };
    return chatReply("after backoff");
  });
  try {
    const out = await openaiSummarize(ep.cfg)({ prompt: "p", maxTokens: 64 });
    assert.equal(out, "after backoff");
    assert.equal(calls, 2);
  } finally {
    await ep.close();
  }
});

test("chatComplete: does NOT retry a 4xx client error", async () => {
  let calls = 0;
  const ep = await startEndpoint(() => {
    calls += 1;
    return { status: 400, json: { error: "bad model" } };
  });
  try {
    await assert.rejects(
      () => openaiSummarize(ep.cfg)({ prompt: "p", maxTokens: 64 }),
      (err: unknown) => err instanceof OpenAICompatRequestError && err.status === 400,
    );
    assert.equal(calls, 1, "4xx is terminal — no retry");
  } finally {
    await ep.close();
  }
});

test("chatComplete: wraps a non-JSON 200 body as OpenAICompatRequestError", async () => {
  const ep = await startEndpoint(() => ({ status: 200, raw: "<html>not json</html>" }));
  try {
    await assert.rejects(
      () => openaiSummarize(ep.cfg)({ prompt: "p", maxTokens: 64 }),
      OpenAICompatRequestError,
    );
  } finally {
    await ep.close();
  }
});

test("openaiSummarize: runs the pre-send redactor over the outgoing prompt", async () => {
  const ep = await startEndpoint(() => chatReply("done"));
  try {
    const preSend = async (text: string): Promise<string> =>
      text.replace(/Ada Lovelace/g, "[REDACTED]");
    await openaiSummarize(
      ep.cfg,
      preSend,
    )({ prompt: "User Ada Lovelace fixed the parser.", maxTokens: 64 });
    const sent = ep.captured[0]?.body.messages?.[1]?.content ?? "";
    assert.equal(sent.includes("Ada Lovelace"), false, "raw name must not leave the box");
    assert.equal(sent.includes("[REDACTED]"), true);
  } finally {
    await ep.close();
  }
});

test("openaiSummarize: strips <think> reasoning and caps at 240 chars", async () => {
  const long = "x".repeat(400);
  const ep = await startEndpoint(() => chatReply(`<think>deciding…</think>${long}`));
  try {
    const out = await openaiSummarize(ep.cfg)({ prompt: "p", maxTokens: 64 });
    assert.equal(out.includes("think"), false);
    assert.equal(out.length, 240);
  } finally {
    await ep.close();
  }
});

test("openaiSummarize: throws OpenAICompatRequestError on a non-2xx reply", async () => {
  const ep = await startEndpoint(() => ({ status: 500, json: { error: "boom" } }));
  try {
    await assert.rejects(
      () => openaiSummarize(ep.cfg)({ prompt: "p", maxTokens: 64 }),
      (err: unknown) => err instanceof OpenAICompatRequestError && err.status === 500,
    );
  } finally {
    await ep.close();
  }
});

test("openaiSummarize: throws when the model returns only a <think> block", async () => {
  const ep = await startEndpoint(() => chatReply("<think>only thinking, no answer</think>"));
  try {
    // Empty-after-strip is a summariser-CONTRACT failure (raised by the
    // shared makeSummarizer), not an HTTP/transport error — so it surfaces
    // as a generic Error. It still THROWS so resilientSummarize degrades.
    await assert.rejects(
      () => openaiSummarize(ep.cfg)({ prompt: "p", maxTokens: 64 }),
      /no answer text/,
    );
  } finally {
    await ep.close();
  }
});

test("openaiCognize: parses JSON tags from the reply", async () => {
  const ep = await startEndpoint(() =>
    chatReply('{"emotions":["focused"],"meta":["in-flow"],"posture":["ship-it"]}'),
  );
  try {
    const tags = await openaiCognize(ep.cfg)({
      abstract: "Refactored the ingest queue to drop the SQLite dependency.",
    });
    assert.deepEqual(tags, { emotions: ["focused"], meta: ["in-flow"], posture: ["ship-it"] });
  } finally {
    await ep.close();
  }
});

test("openaiCognize: returns null for a too-short abstract without calling out", async () => {
  const ep = await startEndpoint(() => chatReply("{}"));
  try {
    const tags = await openaiCognize(ep.cfg)({ abstract: "tiny" });
    assert.equal(tags, null);
    assert.equal(ep.captured.length, 0, "no request should be made for a sub-threshold abstract");
  } finally {
    await ep.close();
  }
});

test("openaiCognize: returns null (best-effort) when the endpoint errors", async () => {
  const ep = await startEndpoint(() => ({ status: 503, json: {} }));
  try {
    const tags = await openaiCognize(ep.cfg)({
      abstract: "A sufficiently long abstract to pass the length guard.",
    });
    assert.equal(tags, null);
  } finally {
    await ep.close();
  }
});

test("defaultOpenAICompatConfig: parses env when base url + model are set", () => {
  const prev = {
    base: process.env.MODELSTAT_LLM_BASE_URL,
    model: process.env.MODELSTAT_LLM_MODEL,
    key: process.env.MODELSTAT_LLM_API_KEY,
  };
  try {
    process.env.MODELSTAT_LLM_BASE_URL = "https://api.example.com/v1";
    process.env.MODELSTAT_LLM_MODEL = "gpt-4o-mini";
    process.env.MODELSTAT_LLM_API_KEY = "sk-abc";
    const cfg = defaultOpenAICompatConfig();
    assert.equal(cfg.baseUrl, "https://api.example.com/v1");
    assert.equal(cfg.model, "gpt-4o-mini");
    assert.equal(cfg.apiKey, "sk-abc");
    assert.ok(cfg.timeoutMs > 0);
  } finally {
    restoreEnv("MODELSTAT_LLM_BASE_URL", prev.base);
    restoreEnv("MODELSTAT_LLM_MODEL", prev.model);
    restoreEnv("MODELSTAT_LLM_API_KEY", prev.key);
  }
});

test("defaultOpenAICompatConfig: throws OpenAICompatConfigError when base url is missing", () => {
  const prev = {
    base: process.env.MODELSTAT_LLM_BASE_URL,
    model: process.env.MODELSTAT_LLM_MODEL,
  };
  try {
    delete process.env.MODELSTAT_LLM_BASE_URL;
    process.env.MODELSTAT_LLM_MODEL = "gpt-4o-mini";
    assert.throws(() => defaultOpenAICompatConfig(), OpenAICompatConfigError);
  } finally {
    restoreEnv("MODELSTAT_LLM_BASE_URL", prev.base);
    restoreEnv("MODELSTAT_LLM_MODEL", prev.model);
  }
});

function restoreEnv(key: string, value: string | undefined): void {
  if (value === undefined) delete process.env[key];
  else process.env[key] = value;
}
