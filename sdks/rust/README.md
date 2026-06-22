# modelstat

**Wrap your backend's LLM calls and get spend + usage analytics — while your prompts stay on your own machine.**

`modelstat` is a privacy-first Rust SDK. It captures the LLM calls your backend already makes and hands them to a **local modelstat daemon**, which **summarizes them on your machine with a local model** and ships only short, **redacted abstracts** to the modelstat analytics server. Raw prompts, completions, and tool arguments **never leave your infrastructure**.

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

- **Privacy by construction.** Summarization happens **on your machine**. Only a bounded, redacted abstract (≤512 chars) + token/cost numbers are uploaded — never raw text. That's what gives you content-level attribution (by project, feature, work-type) *without* sending content to a vendor.
- **No added request latency.** `record()` is a non-blocking move into an in-memory buffer; a background worker handles redaction, the daemon hand-off, batching, and shipping entirely off your request path. If the buffer fills, the newest record is dropped and a counter ticks up — your request is **never** blocked.
- **One daemon, many producers.** Every service instance points at the same local daemon; the daemon owns the local model, durable retry, and the upload. Your app stays a thin, dependency-light client.

## Install

```toml
# Cargo.toml
[dependencies]
modelstat = "0.0.5"
```

## Guide: run a daemon locally, then point the SDK at it

### 1. Run the modelstat daemon

The daemon is the open-source `modelstat` daemon. It runs as a background service, downloads a small local model on first start, and listens on loopback for SDK traffic.

```bash
# zero-install: starts the background service + fetches the local model
npx modelstat@latest

# …or install it globally
npm i -g modelstat && modelstat start

modelstat status      # confirm it's running (and which loopback port it uses)
```

By default the daemon listens on `http://127.0.0.1:4319`.

### 2. Point the SDK at the daemon

Local-daemon mode is the **default** — supply your org ingest key and a source label and you're pointed at the local daemon already:

```rust
use modelstat::{Client, Config};

let cfg = Config::new("msk_live_…", "raw_sdk_openai"); // defaults to the local daemon
let ms = Client::new(cfg);
```

Changed the daemon's port? Set the mode explicitly (`mode` is a public field):

```rust
use modelstat::Mode;

let mut cfg = Config::new("msk_live_…", "raw_sdk_openai");
cfg.mode = Mode::LocalDaemon { url: "http://127.0.0.1:4319/v1/ingest".into() };
```

### 3. Record your calls

After each real LLM call returns, hand the SDK what it already has:

```rust
use modelstat::{LlmCall, TokenUsage};

ms.record(
    LlmCall::new("openai", "session-or-trace-id")     // provider, grouping id
        .model("gpt-x")
        .tokens(TokenUsage { input: 800, output: 120, ..Default::default() })
        .text("the prompt", "the completion"),         // raw — summarized locally, never uploaded raw
);

ms.shutdown().await;   // flush what's buffered on the way out
```

**What flows where:** your prompt + completion go to the **local daemon only**. The daemon summarizes them with its local model, redacts, and uploads just the abstract + token/cost metadata to modelstat. The `source` label (`raw_sdk_openai`) records which integration produced the calls; `session_id` groups calls into a conversation/session downstream.

## Metadata tags (attribution)

Attach free-form `string → string` tags to attribute spend — by `feature`, `customer_id`, `team`, `environment`, whatever you slice on. Two layers merge, **per-call winning**: `Config` defaults (constant across every call) sit underneath, and per-call tags override them on a shared key.

```rust
use modelstat::{Config, LlmCall};

// Constant tags on every call:
let cfg = Config::new("msk_live_…", "raw_sdk_openai")
    .with_metadata("environment", "prod")
    .with_metadata("service", "checkout");

// Per-call tags (override the defaults on a shared key):
ms.record(
    LlmCall::new("openai", "trace-123")
        .metadata("feature", "search")
        .with_metadata([("customer_id", "cus_42"), ("team", "growth")]),
);
```

Caps are enforced in-process before anything ships: at most **16** entries (excess keys dropped deterministically in sorted-key order), keys truncated to **64** chars, values to **256**. The merged map ships as the event's `metadata` field, omitted entirely when empty.

> **No ambient layer in Rust.** The TypeScript and Python SDKs add a middle "ambient context" tier (`withMetadata` / `with modelstat.metadata(...)`) built on task-locals; the Rust equivalent (task-locals) is awkward and easy to misuse across `.await` points, so the Rust SDK intentionally ships only the two layers above (`Config` defaults + per-call). Set a per-call tag where you'd otherwise reach for ambient context.

## Modes

| Mode | Where summarization runs | What leaves your machine | Use when |
|---|---|---|---|
| **Local daemon** *(default)* | Your machine (daemon's local model) | Redacted abstract + metadata only | Maximum privacy; a daemon can run on/near the host |
| **Remote** | modelstat server | Floor-redacted full turns (`raw=true`), or just the ≤320-char redacted excerpt (`raw=false`) | Serverless / can't run a local model; you accept server-side summarization |

```rust
// Remote (no local daemon / no local model):
let cfg = Config::new("msk_live_…", "raw_sdk_openai")
    .with_remote("https://api.modelstat.ai", /* raw */ true);
```

## Taxonomy auto-detection (off by default)

modelstat can auto-detect a work-type *taxonomy* over your sessions, but that's tuned for interactive coding sessions — backend LLM usage usually isn't. So for the SDKs taxonomy is **off by default**: every batch ships an explicit `auto_taxonomy: false`. Opt in by setting the config flag:

```rust
let mut cfg = Config::new("msk_live_…", "raw_sdk_openai");
cfg.auto_taxonomy = true; // force server-side taxonomy auto-detection on
```

## Privacy floor (always on)

Before any bytes leave the SDK process — in **every** mode — an in-process redaction floor scrubs secrets (provider keys, tokens, JWTs, PEM blocks, DB passwords, …), emails, and absolute home paths. "Raw" mode means *full turns*, not *leaked credentials* — the floor still runs. Tool calls ship only hashes, byte sizes, and allowlisted command verbs — never raw args, results, paths, or command text.

## What's live today (v0.0.5)

Early release — the honest state, so nothing surprises you:

- ✅ **SDK**: zero-latency capture, the redaction floor, batching/backpressure, and both transports are implemented and tested.
- ✅ **Remote mode** is live end-to-end: `raw = false` ships the ≤320-char redacted excerpt to `/v1/ingest`, and `raw = true` ships full floor-redacted turns to `/v1/ingest/raw`, which summarizes them server-side and persists only the abstract (raw is never stored). Authenticate with an org ingest key (`msk_…`).
- 🚧 **Daemon loopback ingest** (the receiving side of local-daemon mode) is in active development. The daemon already runs a local model and summarizes today; the SDK-push endpoint is landing next. **Until it ships, use remote mode** — the local-daemon API is stable, so your code won't change when it does.

Progress: https://github.com/modelstat/modelstat

## License

Apache-2.0.
