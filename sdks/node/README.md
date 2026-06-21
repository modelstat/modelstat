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

## Privacy floor (always on)

Before any bytes leave the SDK process — in **every** mode, including `raw = true` — an in-process redaction floor scrubs:

- **Secrets:** Anthropic / OpenAI / Google API keys, AWS access + secret keys, GitHub PAT/OAuth/app tokens, Slack tokens, Stripe live/test keys, Discord tokens, JWTs, PEM private-key blocks, modelstat device secrets, generic `NAME_TOKEN=…`/`*_SECRET=…` env values (the name is kept, the value redacted), `Bearer` tokens, and DB-connection-string passwords.
- **PII:** email addresses and absolute home paths (`/Users/…`, `/home/…`, `C:\Users\…`).

Secrets are replaced with `[REDACTED:<kind>]`; emails with `[REDACTED:email]`; paths with `[REDACTED:path]`. "Raw" mode means *full turns*, not *leaked credentials* — the floor still runs.

You can opt out of in-process redaction with `cfg.redaction = "none"`, but only do so when shipping to a trusted local daemon that redacts, or under an explicit raw-data contract.

## Determinism & dedupe

Source-event and batch ids are derived with **blake3** (via `@noble/hashes`), exactly matching the modelstat server's algorithm, so a resend of the same calls reuses the same ids and the server's manifest dedupes them. Tool-call arg hashes use **sha256**.

## API surface

- `new Config(ingestKey, agent)` — `.withRemote(baseUrl, raw)`, `.withDeviceId(id)`; public fields `mode`, `redaction`, `bufferCapacity`, `flushIntervalMs`, `flushMaxBatch`, `deviceId`, `version`.
- `new LlmCall(provider, sessionId)` — fluent `.model(m)`, `.tokens({…})`, `.text(prompt, completion)`; public fields `kind`, `startedAt`, `durationMs`, `cwd`, `git`, `pricing_mode`, `toolCalls`.
- `new Client(cfg)` / `Client.withTransport(cfg, transport)` — `record(call)`, `await flush()`, `await shutdown()`, `dropped()`.
- `FakeTransport` — an in-memory transport for tests (`.batches()`).
- `redact(text)` — run the floor directly; returns `{ text, secrets, pii }`.

## License

Apache-2.0.
