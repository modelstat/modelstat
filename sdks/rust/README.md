# modelstat (Rust SDK)

Privacy-first SDK for wrapping the LLM calls your backend already makes and
shipping **redacted** usage to modelstat — without adding latency to live
requests.

## How it works

The SDK is *in* your request path, so it sees the exact prompt, completion,
token usage, latency, and tool calls. But it must never slow a live request, so:

- **Hot path** (`Client::record`) does nothing but move your already-in-hand
  call into a bounded in-memory buffer and return.
- A **background worker** redacts, batches, and ships — entirely off the request
  path. On buffer overflow the newest record is dropped and a counter
  increments; your request is never blocked and memory never grows unbounded.

## Modes

- **Local daemon (default).** The SDK hands calls to a local **modelstat daemon**
  over loopback. The daemon summarizes with its local Qwen model and ships only
  redacted abstracts to the server — **raw text never leaves the machine.**
- **Remote.** Ship directly to the modelstat server — no local daemon, no local
  model. With `raw = true`, send full (still floor-redacted) turns for
  **server-side** summarization.

Default is the local daemon unless you explicitly configure remote.

## Privacy floor

Before any bytes leave the SDK process, an in-process **redaction floor** scrubs
secrets (provider keys, tokens, JWTs, PEM blocks, DB passwords, …), emails, and
absolute home paths. The floor runs **even in raw mode** — "raw" means *full
turns*, not *leaked credentials*. Tool calls ship only hashes, byte sizes, and
allowlisted command verbs — never raw args, results, paths, or command text.

## Quickstart

```rust
use modelstat::{Client, Config, LlmCall, TokenUsage};

// Org-scoped ingest key binds traffic to your account.
let cfg = Config::new("msk_live_…", "raw_sdk_openai")
    .with_remote("https://api.modelstat.ai", /* raw */ true);
let ms = Client::new(cfg);

// ... after your real LLM call returns ...
ms.record(
    LlmCall::new("openai", "session-or-trace-id")
        .model("gpt-x")
        .tokens(TokenUsage { input: 800, output: 120, ..Default::default() })
        .text("the prompt", "the completion"),
);

ms.shutdown().await; // flush on the way out
```

`source` (the second `Config::new` argument, e.g. `raw_sdk_openai`) labels what
produced the calls; `session_id` groups calls into a conversation/session
downstream.

## Status

Prototype. Implemented: zero-latency capture, the redaction floor, batching, the
local-daemon and remote transports, and the wire contract. Not yet: durable
on-disk retry for the remote path (the local **daemon** already owns durable
retry in daemon mode), provider auto-wrappers (`wrap(client)`), and the
`/v1/ingest/raw` server endpoint that backs `raw = true`.

License: Apache-2.0.
