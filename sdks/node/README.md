# @modelstat/sdk

**Wrap your backend's LLM calls and get spend + usage analytics — while your prompts stay on your own machine.**

`@modelstat/sdk` is a privacy-first SDK for Node.js (≥20). It captures the LLM calls your backend already makes and hands them to a **local modelstat daemon**, which **summarizes them on your machine with a local model** and ships only short, **redacted abstracts** to the modelstat analytics server. Raw prompts, completions, and tool arguments **never leave your infrastructure**.

```text
   your backend                          your machine                       modelstat
 ┌──────────────┐   loopback        ┌──────────────────────┐   HTTPS    ┌───────────────┐
 │  ms.record() │ ───────────────▶  │   modelstat daemon   │ ─────────▶ │   analytics   │
 │ (non-block)  │   raw stays here  │  • local model        │  redacted  │   dashboard   │
 └──────────────┘                   │    → summarize        │  abstract  │  (spend, by   │
        ▲                           │  • redact (PII/keys)  │   + tokens │  project/etc) │
   real LLM call                    │  • batch + retry      │            └───────────────┘
                                    └──────────────────────┘
              ↑ raw prompts / completions / args never cross this line ↑
```

<!-- dashboard screenshot: drop assets/activities-screenshot.png here when available -->

## Why a local daemon?

- **Privacy by construction.** Summarization happens **on your machine**. Only a bounded, redacted abstract + token/cost numbers are uploaded — never raw text. That is what gives you content-level attribution (by project, feature, work-type) *without* sending content to a vendor.
- **No added request latency.** `record()` is a non-blocking push into an in-memory buffer; a background worker handles redaction, the daemon hand-off, batching, and shipping entirely off your request path. If the buffer fills, the newest record is dropped and a counter ticks up — your request is **never** blocked.
- **One daemon, many producers.** Every service instance points at the same local daemon; the daemon owns the local model, durable retry, and the upload. Your app stays a thin, dependency-light client.

## Install

```bash
npm install @modelstat/sdk
```

The only runtime dependency is [`@noble/hashes`](https://github.com/paulmillr/noble-hashes) (pure JS — no native build). Requires Node ≥20 (uses the global `fetch`).

## Quick start (local-daemon mode, the default)

Local-daemon mode is the **default** — supply your org ingest key and an agent label and you are already pointed at the local daemon on loopback:

```ts
import { Client, Config, LlmCall } from "@modelstat/sdk";

// `msk_…` org ingest key + the AI-tool label this service integrates with.
const cfg = new Config("msk_live_…", "raw_sdk_openai"); // defaults to the local daemon
const ms = new Client(cfg);

// ... after each real LLM call returns, hand the SDK what it already has ...
ms.record(
  new LlmCall("openai", "session-or-trace-id") // provider, grouping id
    .model("gpt-x")
    .tokens({ input: 800, output: 120 })
    .text("the prompt", "the completion"), // raw — summarized locally, never uploaded raw
);

await ms.shutdown(); // flush what's buffered on the way out
```

The daemon listens on `http://127.0.0.1:4319` by default. Changed the port? Set the mode explicitly:

```ts
const cfg = new Config("msk_live_…", "raw_sdk_openai");
cfg.mode = { kind: "local_daemon", url: "http://127.0.0.1:4319/v1/ingest" };
```

`record()` returns immediately. A background worker batches (every 2s or every 256 records, whichever comes first), redacts, and ships. `ms.dropped()` reports how many records were dropped under backpressure.

## Remote mode (no local daemon)

For serverless or anywhere you cannot run a local model, ship directly to the modelstat server:

```ts
const cfg = new Config("msk_live_…", "raw_sdk_openai")
  .withRemote("https://api.modelstat.ai", /* raw */ true);
const ms = new Client(cfg);
```

| Mode | Where summarization runs | What leaves your machine |
|---|---|---|
| **Local daemon** *(default)* | Your machine (daemon's local model) | Redacted abstract + metadata only |
| **Remote, `raw = true`** | modelstat server | Floor-redacted **full turns** → `/v1/ingest/raw` |
| **Remote, `raw = false`** | (no summarization) | Just the floor-redacted **≤320-char** excerpt → `/v1/ingest` |

## Auto-recording with `wrap()`

Don't want to hand-build an `LlmCall`? Wrap your `openai` or `@anthropic-ai/sdk` client and keep using it exactly as before — each completion call is forwarded untouched and auto-recorded after it returns:

```ts
import OpenAI from "openai";
import { Client, Config, wrap } from "@modelstat/sdk";

const ms = new Client(new Config("msk_live_…", "raw_sdk_openai"));
const openai = wrap(new OpenAI(), { client: ms, metadata: { feature: "search" } });

// …unchanged usage; this is auto-recorded (provider, model, tokens, text)…
const resp = await openai.chat.completions.create({
  model: "gpt-x",
  messages: [{ role: "user", content: "hello" }],
});
```

`wrap(anthropicClient, { client: ms })` works the same for `@anthropic-ai/sdk` (it reads `messages.create`, and `usage.input_tokens` / `usage.output_tokens`). Recording is **best-effort**: it never alters the wrapped call's result or timing, and a recording fault is swallowed rather than surfaced. The wrap helper detects the client *structurally* — it does **not** import `openai`/`@anthropic-ai/sdk`, so they stay optional peer dependencies. `metadata` here is a wrap-default (the per-call layer); `Config` defaults and the ambient layer still apply underneath. Pass `sessionId` (a string or `() => string`) to set the grouping id.

## Metadata tags (attribution)

Attach free-form `string → string` tags to attribute spend — by `feature`, `customer_id`, `team`, `environment`, whatever you slice on. Three layers merge, **later layer wins**: `Config` defaults (constant) < an ambient context layer < per-call tags.

```ts
import { Config, LlmCall, withMetadata } from "@modelstat/sdk";

// 1. Config defaults — on every call.
const cfg = new Config("msk_live_…", "raw_sdk_openai");
cfg.metadata = { environment: "prod", service: "checkout" };

// 2. Ambient layer — scoped to an async region (auto-resets on exit/throw).
await withMetadata({ customer_id: "cus_42" }, async () => {
  // 3. Per-call — overrides the layers above on a shared key.
  ms.record(new LlmCall("openai", "trace-123").metadata({ feature: "search" }));
});
```

`withMetadata` uses `AsyncLocalStorage`, so any `record()` (or `wrap()` call) inside the region — even across `await`s — picks up the ambient tags; nested scopes merge. Caps are enforced in-process before send: at most **16** entries (excess keys dropped deterministically in sorted-key order), keys truncated to **64** chars, values to **256**. The merged map ships as the event's `metadata` field, omitted entirely when empty.

## Capturing tool calls

If your call invoked tools, attach them — the SDK hashes and sizes the args, and ships **only** hashes, byte counts, and up to three allowlisted command verbs. Raw args, results, paths, and command text never ship.

```ts
const call = new LlmCall("anthropic", "trace-123").tokens({ input: 1200, output: 300 });
call.toolCalls.push({
  name: "Bash",
  server: "builtin", // or "mcp:<server>"
  args: { command: "ls -la", timeout: 5 }, // hashed + sized, never shipped
  resultBytes: 482,
  status: "success",
  commandFamilies: ["ls"],
});
ms.record(call);
```

## Taxonomy auto-detection (off by default)

modelstat can auto-detect a work-type *taxonomy* over your sessions, but that's tuned for interactive coding sessions — backend LLM usage usually isn't. So for the SDK taxonomy is **off by default**: every batch ships an explicit `auto_taxonomy: false`. Opt in with the config flag:

```ts
const cfg = new Config("msk_live_…", "raw_sdk_openai");
cfg.autoTaxonomy = true; // force server-side taxonomy auto-detection on
```

## Privacy floor (always on)

Before any bytes leave the SDK process — in **every** mode, including `raw = true` — an in-process redaction floor scrubs:

- **Secrets:** Anthropic / OpenAI / Google API keys, AWS access + secret keys, GitHub PAT/OAuth/app tokens, Slack tokens, Stripe live/test keys, Discord tokens, JWTs, PEM private-key blocks, modelstat device secrets, generic `NAME_TOKEN=…`/`*_SECRET=…` env values (the name is kept, the value redacted), `Bearer` tokens, and DB-connection-string passwords.
- **PII:** email addresses and absolute home paths (`/Users/…`, `/home/…`, `C:\Users\…`).

Secrets are replaced with `[REDACTED:<kind>]`; emails with `[REDACTED:email]`; paths with `[REDACTED:path]`. "Raw" mode means *full turns*, not *leaked credentials* — the floor still runs.

You can opt out of in-process redaction with `cfg.redaction = "none"`, but only do so when shipping to a trusted local daemon that redacts, or under an explicit raw-data contract.

## Determinism & dedupe

Source-event and batch ids are derived with **blake3** (via `@noble/hashes`), exactly matching the modelstat server's algorithm, so a resend of the same calls reuses the same ids and the server's manifest dedupes them. Tool-call arg hashes use **sha256**.

## API surface

- `new Config(ingestKey, agent)` — `.withRemote(baseUrl, raw)`, `.withDeviceId(id)`; public fields `mode`, `redaction`, `bufferCapacity`, `flushIntervalMs`, `flushMaxBatch`, `deviceId`, `version`, `autoTaxonomy` (default `false`), `metadata` (default `{}`).
- `new LlmCall(provider, sessionId)` — fluent `.model(m)`, `.tokens({…})`, `.text(prompt, completion)`, `.metadata({…})`; public fields `kind`, `startedAt`, `durationMs`, `cwd`, `git`, `pricing_mode` (`"subscription"` | `"api"` | `"unknown"`, default `"unknown"` — set it, or your spend is reported as unattributed rather than billed), `toolCalls`, `metadataTags`.
- `new Client(cfg)` / `Client.withTransport(cfg, transport)` — `record(call)`, `await flush()`, `await shutdown()`, `dropped()`.
- `wrap(providerClient, { client, metadata?, sessionId? })` — auto-record an `openai` / `@anthropic-ai/sdk` client (returns a transparent proxy).
- `withMetadata(tags, fn)` — run `fn` with ambient attribution tags in scope.
- `capMetadata(map)` + `METADATA_MAX_ENTRIES` / `METADATA_MAX_KEY_CHARS` / `METADATA_MAX_VALUE_CHARS` — the client-side caps.
- `FakeTransport` — an in-memory transport for tests (`.batches()`).
- `redact(text)` — run the floor directly; returns `{ text, secrets, pii }`.

## License

Apache-2.0.
