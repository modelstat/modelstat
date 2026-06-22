# modelstat

**Wrap your backend's LLM calls and get spend + usage analytics — while your prompts stay on your own machine.**

`modelstat-sdk` is a privacy-first Python SDK. It captures the LLM calls your backend already makes and hands them to a **local modelstat daemon**, which **summarizes them on your machine with a local model** and ships only short, **redacted abstracts** to the modelstat analytics server. Raw prompts, completions, and tool arguments **never leave your infrastructure**.

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

- **Privacy by construction.** Summarization happens **on your machine**. Only a bounded, redacted abstract + token/cost numbers are uploaded — never raw text. That's what gives you content-level attribution (by project, feature, work-type) *without* sending content to a vendor.
- **No added request latency.** `record()` is a non-blocking enqueue into an in-memory buffer; a background worker **thread** handles redaction, the daemon hand-off, batching, and shipping entirely off your request path. If the buffer fills, the newest record is dropped and a counter ticks up — your request is **never** blocked.
- **One daemon, many producers.** Every service instance points at the same local daemon; the daemon owns the local model, durable retry, and the upload. Your app stays a thin, dependency-light client (one runtime dependency: `blake3`).

## Install

```bash
pip install modelstat-sdk
```

```python
import modelstat
```

The import package is `modelstat`; the distribution on PyPI is `modelstat-sdk`. Requires Python 3.9+.

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

Local-daemon mode is the **default** — supply your org ingest key and an agent label and you're pointed at the local daemon already:

```python
from modelstat import Client, Config

cfg = Config("msk_live_…", "raw_sdk_openai")  # defaults to the local daemon
ms = Client(cfg)
```

Changed the daemon's port? Set the mode explicitly:

```python
from modelstat import Config, Mode

cfg = Config("msk_live_…", "raw_sdk_openai")
cfg.mode = Mode.local_daemon("http://127.0.0.1:4319/v1/ingest")
```

### 3. Record your calls

After each real LLM call returns, hand the SDK what it already has. `record()` is non-blocking; use the client as a context manager so it flushes on the way out:

```python
from modelstat import Client, Config, LlmCall, TokenUsage

cfg = Config("msk_live_…", "raw_sdk_openai")

with Client(cfg) as ms:                                  # shutdown() flushes on exit
    ms.record(
        LlmCall("openai", "session-or-trace-id")          # provider, grouping id
        .model_("gpt-x")
        .with_tokens(TokenUsage(input=800, output=120))
        .text("the prompt", "the completion")             # raw — summarized locally, never uploaded raw
    )
```

You can also construct an `LlmCall` with plain keyword arguments
(`LlmCall(provider="openai", session_id="…", model="gpt-x", tokens=TokenUsage(input=800))`).

Call `ms.flush()` to block until buffered calls are shipped, `ms.shutdown()` to flush and stop the worker thread, and `ms.dropped()` to read the overflow counter.

**What flows where:** your prompt + completion go to the **local daemon only**. The daemon summarizes them with its local model, redacts, and uploads just the abstract + token/cost metadata to modelstat. The `agent` label (`raw_sdk_openai`) records which integration produced the calls; `session_id` groups calls into a conversation/session downstream.

## Auto-recording with `wrap()`

Don't want to hand-build an `LlmCall`? Wrap your OpenAI or Anthropic client and keep using it exactly as before — each completion call is forwarded untouched and auto-recorded after it returns:

```python
from openai import OpenAI
import modelstat
from modelstat import Client, Config

ms = Client(Config("msk_live_…", "raw_sdk_openai"))
client = modelstat.wrap(OpenAI(), recorder=ms, metadata={"feature": "search"})

# …unchanged usage; this is auto-recorded (provider, model, tokens, text)…
resp = client.chat.completions.create(
    model="gpt-x", messages=[{"role": "user", "content": "hello"}]
)
```

`modelstat.wrap(anthropic_client, recorder=ms)` works the same for the `anthropic` SDK (it reads `messages.create`, and `usage.input_tokens` / `usage.output_tokens`). Recording is **best-effort**: it never alters the wrapped call's result or timing, and a recording fault is swallowed rather than surfaced. The wrap helper detects the client *dynamically* — it does **not** import `openai`/`anthropic`, so they stay optional peers. `metadata=` here is a wrap-default (the per-call layer); `Config` defaults and the ambient layer still apply underneath. Sync clients are fully supported; `AsyncOpenAI` / `AsyncAnthropic` are too (the coroutine is awaited and recorded on completion). Pass `session_id=` (a string or a zero-arg callable) to set the grouping id.

## Metadata tags (attribution)

Attach free-form `str → str` tags to attribute spend — by `feature`, `customer_id`, `team`, `environment`, whatever you slice on. Three layers merge, **later layer wins**: `Config` defaults (constant) < an ambient context layer < per-call tags.

```python
import modelstat
from modelstat import Config, LlmCall

# 1. Config defaults — on every call.
cfg = Config("msk_live_…", "raw_sdk_openai")
cfg.metadata = {"environment": "prod", "service": "checkout"}

# 2. Ambient layer — scoped to a `with` block (auto-resets on exit/exception).
with modelstat.metadata({"customer_id": "cus_42"}):
    # 3. Per-call — overrides the layers above on a shared key.
    ms.record(LlmCall("openai", "trace-123", metadata={"feature": "search"}))
    # …or merge with .with_metadata({...}).
```

`modelstat.metadata(...)` uses `contextvars`, so any `record()` (or `wrap()` call) inside the block — including across `await`s under asyncio — picks up the ambient tags; nested blocks merge. Caps are enforced in-process before send: at most **16** entries (excess keys dropped deterministically in sorted-key order), keys truncated to **64** chars, values to **256**. The merged map ships as the event's `metadata` field, omitted entirely when empty.

## Modes

| Mode | Where summarization runs | What leaves your machine | Use when |
|---|---|---|---|
| **Local daemon** *(default)* | Your machine (daemon's local model) | Redacted abstract + metadata only | Maximum privacy; a daemon can run on/near the host |
| **Remote** | modelstat server | Floor-redacted full turns (`raw=True`), or just the ≤320-char redacted excerpt (`raw=False`) | Serverless / can't run a local model; you accept server-side summarization |

```python
# Remote (no local daemon / no local model):
cfg = Config("msk_live_…", "raw_sdk_openai").with_remote(
    "https://api.modelstat.ai", raw=True
)
```

## Taxonomy auto-detection (off by default)

modelstat can auto-detect a work-type *taxonomy* over your sessions, but that's tuned for interactive coding sessions — backend LLM usage usually isn't. So for the SDK taxonomy is **off by default**: every batch ships an explicit `auto_taxonomy: false`. Opt in with the config flag:

```python
cfg = Config("msk_live_…", "raw_sdk_openai")
cfg.auto_taxonomy = True  # force server-side taxonomy auto-detection on
```

## Privacy floor (always on)

Before any bytes leave the SDK process — in **every** mode — an in-process redaction floor scrubs secrets (provider keys, tokens, JWTs, PEM blocks, DB passwords, …), emails, and absolute home paths. "Raw" mode means *full turns*, not *leaked credentials* — the floor still runs. Tool calls ship only hashes, byte sizes, and allowlisted command verbs — never raw args, results, paths, or command text.

What the floor redacts: Anthropic / OpenAI / Google / AWS / GitHub / Slack / Stripe / Discord keys and tokens, JWTs, PEM private-key blocks, modelstat device secrets, generic `NAME_KEY=value` env secrets (the name is kept, the value is dropped), `Bearer` tokens, database-URL passwords, lone 40-char AWS-style secret blobs, email addresses, and absolute `/Users/…`, `/home/…`, and `C:\Users\…` paths.

## What's live today (v0.0.3)

Early release — the honest state, so nothing surprises you:

- ✅ **SDK**: zero-latency capture, the redaction floor, batching/backpressure, and both transports are implemented and tested.
- 🚧 **Daemon loopback ingest** (the receiving side of local-daemon mode) is in active development. The daemon already runs a local model and summarizes today; the SDK-push endpoint is landing next. **Until it ships, use remote mode** — the local-daemon API is stable, so your code won't change when it does.
- 🚧 **`/v1/ingest/raw`** (server-side summarization for `raw=True`) is rolling out; `raw=False` against `/v1/ingest` works today for token/cost telemetry.

Progress: https://github.com/modelstat/modelstat

## License

Apache-2.0.
