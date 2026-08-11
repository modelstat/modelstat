import assert from "node:assert/strict";
import test from "node:test";
import { Client, Config, FakeTransport, wrap } from "../index.js";

const cfg = (agent = "raw_sdk_openai"): Config =>
  new Config("msk_test", agent).withDeviceId("dev_test");

// ---- fake provider clients (OpenAI- and Anthropic-shaped) -------------------

interface FakeCall {
  args: unknown;
}

/** A minimal stand-in for `new OpenAI()` — `chat.completions.create`. */
function fakeOpenAI(response: unknown, calls: FakeCall[]) {
  return {
    // an unrelated nested method, to prove pass-through still works
    models: {
      list(): string {
        return "models.list";
      },
    },
    chat: {
      completions: {
        create(args: unknown): Promise<unknown> {
          calls.push({ args });
          return Promise.resolve(response);
        },
      },
    },
  };
}

/** A minimal stand-in for `new Anthropic()` — `messages.create`. */
function fakeAnthropic(response: unknown, calls: FakeCall[]) {
  return {
    messages: {
      create(args: unknown): Promise<unknown> {
        calls.push({ args });
        return Promise.resolve(response);
      },
    },
  };
}

test("wrap(openai) auto-records one call with provider/model/tokens and returns the response untouched", async () => {
  const calls: FakeCall[] = [];
  const response = {
    model: "gpt-x",
    usage: { prompt_tokens: 800, completion_tokens: 120 },
    choices: [{ message: { role: "assistant", content: "the completion" } }],
  };
  const fake = new FakeTransport();
  const ms = Client.withTransport(cfg("raw_sdk_openai"), fake);
  const openai = wrap(fakeOpenAI(response, calls), { client: ms });

  const out = await openai.chat.completions.create({
    model: "gpt-x",
    messages: [{ role: "user", content: "the prompt" }],
  });

  // Underlying response is returned unchanged (same reference).
  assert.equal(out, response);
  // The real call ran exactly once.
  assert.equal(calls.length, 1);

  await ms.flush();
  const events = fake.batches().flatMap((b) => b.events);
  assert.equal(events.length, 1);
  const ev = events[0]!;
  assert.equal(ev.provider, "openai");
  assert.equal(ev.model, "gpt-x");
  assert.equal(ev.tokens.input, 800);
  assert.equal(ev.tokens.output, 120);
  // Prompt + completion were captured (and redacted/capped through the floor).
  assert.ok(ev.content_excerpt!.includes("the prompt"));
  assert.ok(ev.content_excerpt!.includes("the completion"));
  await ms.shutdown();
});

test("wrap(anthropic) auto-records one call with the Anthropic token shape", async () => {
  const calls: FakeCall[] = [];
  const response = {
    model: "claude-x",
    usage: { input_tokens: 1200, output_tokens: 300 },
    content: [{ type: "text", text: "hi there" }],
  };
  const fake = new FakeTransport();
  const ms = Client.withTransport(cfg("raw_sdk_anthropic"), fake);
  const anthropic = wrap(fakeAnthropic(response, calls), { client: ms });

  const out = await anthropic.messages.create({
    model: "claude-x",
    system: "be terse",
    messages: [{ role: "user", content: "hello" }],
  });

  assert.equal(out, response);
  assert.equal(calls.length, 1);

  await ms.flush();
  const events = fake.batches().flatMap((b) => b.events);
  assert.equal(events.length, 1);
  const ev = events[0]!;
  assert.equal(ev.provider, "anthropic");
  assert.equal(ev.model, "claude-x");
  assert.equal(ev.tokens.input, 1200);
  assert.equal(ev.tokens.output, 300);
  await ms.shutdown();
});

test("pass-through: unrelated properties/methods forward to the real client", async () => {
  const calls: FakeCall[] = [];
  const ms = Client.withTransport(cfg(), new FakeTransport());
  const openai = wrap(fakeOpenAI({}, calls), { client: ms });
  // Reaching a non-intercepted method still works.
  assert.equal((openai as { models: { list(): string } }).models.list(), "models.list");
  await ms.shutdown();
});

test("a recording failure never breaks the wrapped call", async () => {
  const calls: FakeCall[] = [];
  const response = { model: "gpt-x", usage: {}, choices: [] };
  const ms = Client.withTransport(cfg(), new FakeTransport());
  // Sabotage record() so extraction/record throws.
  (ms as unknown as { record: (...a: unknown[]) => void }).record = () => {
    throw new Error("record boom");
  };
  const openai = wrap(fakeOpenAI(response, calls), { client: ms });

  // The call must still return the real response despite record() throwing.
  const out = await openai.chat.completions.create({
    model: "gpt-x",
    messages: [],
  });
  assert.equal(out, response);
  assert.equal(calls.length, 1);
  await ms.shutdown();
});

test("wrap-default metadata rides the recorded call (and Config defaults still apply)", async () => {
  const calls: FakeCall[] = [];
  const response = {
    model: "gpt-x",
    usage: { prompt_tokens: 1, completion_tokens: 1 },
    choices: [{ message: { content: "ok" } }],
  };
  const c = cfg();
  c.metadata = { environment: "prod" };
  const fake = new FakeTransport();
  const ms = Client.withTransport(c, fake);
  const openai = wrap(fakeOpenAI(response, calls), {
    client: ms,
    metadata: { feature: "search" },
  });

  await openai.chat.completions.create({ model: "gpt-x", messages: [] });
  await ms.flush();

  const md = fake.batches()[0]!.events[0]!.metadata!;
  assert.equal(md.environment, "prod"); // Config default
  assert.equal(md.feature, "search"); // wrap-default (per-call layer)
  await ms.shutdown();
});

test("wrap throws on a client that is neither OpenAI- nor Anthropic-shaped", async () => {
  const ms = Client.withTransport(cfg(), new FakeTransport());
  assert.throws(() => wrap({ foo: "bar" }, { client: ms }), /could not detect/);
  await ms.shutdown();
});

test("a rejected provider call rejects untouched and records nothing", async () => {
  const fake = new FakeTransport();
  const ms = Client.withTransport(cfg(), fake);
  const erroring = {
    chat: {
      completions: {
        create(_args: unknown): Promise<unknown> {
          return Promise.reject(new Error("upstream 500"));
        },
      },
    },
  };
  const openai = wrap(erroring, { client: ms });
  await assert.rejects(
    openai.chat.completions.create({ model: "gpt-x", messages: [] }),
    /upstream 500/,
  );
  await ms.flush();
  assert.equal(fake.batches().length, 0); // failures are not recorded
  await ms.shutdown();
});

test("a wrapped call is dated when the request went out, not when it came back", async () => {
  const calls: FakeCall[] = [];
  // A provider that takes measurable time to answer. Recording happens after
  // the response resolves, so a call built there would carry the LATER instant.
  const slowOpenAI = {
    chat: {
      completions: {
        async create(args: unknown): Promise<unknown> {
          calls.push({ args });
          await new Promise((r) => setTimeout(r, 25));
          return { model: "gpt-x", usage: { prompt_tokens: 1, completion_tokens: 1 } };
        },
      },
    },
  };
  const fake = new FakeTransport();
  const ms = Client.withTransport(cfg(), fake);
  const openai = wrap(slowOpenAI, { client: ms });

  const before = Date.now();
  await openai.chat.completions.create({ model: "gpt-x", messages: [] });
  const after = Date.now();
  await ms.flush();

  const event = fake.batches()[0]!.events[0]!;
  const started = Date.parse(event.started_at!);
  assert.equal(event.ts, event.started_at, "ts carries the same instant");
  assert.ok(started >= before, `${event.started_at} predates the call`);
  assert.ok(
    started < after - 20,
    `${event.started_at} is the response instant, not the request instant`,
  );
  // The interceptor sees one whole response, never a first chunk — so it states
  // no first-token instant rather than inventing one.
  assert.ok(!("first_token_at" in event));
});
