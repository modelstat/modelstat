# modelstat SDKs

**Wrap the LLM calls your backend already makes and get spend + usage analytics — while your prompts stay on your own machine.**

A modelstat SDK captures each LLM call your app makes, **redacts and summarizes it locally**, and ships only short, redacted abstracts (plus token/cost numbers) to the modelstat analytics dashboard. Raw prompts, completions, and tool arguments **never leave your infrastructure**. Calling `record()` is a non-blocking buffer push — it adds **no latency** to your live requests.

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

## Pick your language

| Language | Package | Install | Source |
|---|---|---|---|
| **Rust** | [`modelstat`](https://crates.io/crates/modelstat) (crates.io) | `cargo add modelstat` | [`sdks/rust`](./rust) |
| **TypeScript / Node** | [`@modelstat/sdk`](https://www.npmjs.com/package/@modelstat/sdk) (npm) | `npm i @modelstat/sdk` | [`sdks/node`](./node) |
| **Python** | [`modelstat-sdk`](https://pypi.org/project/modelstat-sdk/) (PyPI) | `pip install modelstat-sdk` | [`sdks/python`](./python) |

All three share one design, one wire contract, and the **same redaction floor** — pick the one your backend is written in.

## Quickstart

The shape is identical everywhere: build a `Config` with your **org ingest key** (`msk_…`, minted from the modelstat dashboard) and a **source label** (which integration is producing the calls), create a `Client`, then `record()` each call after it returns and `flush`/`shutdown` on the way out.

### Rust

```rust
use modelstat::{Client, Config, LlmCall, TokenUsage};

let cfg = Config::new("msk_live_…", "raw_sdk_openai")
    .with_remote("https://api.modelstat.ai", /* raw */ true);
let ms = Client::new(cfg);

// …after your real LLM call returns…
ms.record(
    LlmCall::new("openai", "session-or-trace-id")
        .model("gpt-x")
        .tokens(TokenUsage { input: 800, output: 120, ..Default::default() })
        .text("the prompt", "the completion"),
);

ms.shutdown().await; // flush what's buffered on the way out
```

### TypeScript / Node

```ts
import { Client, Config, LlmCall } from "@modelstat/sdk";

const cfg = new Config("msk_live_…", "raw_sdk_openai")
  .withRemote("https://api.modelstat.ai", /* raw */ true);
const ms = new Client(cfg);

// …after your real LLM call returns…
ms.record(
  new LlmCall("openai", "session-or-trace-id")
    .model("gpt-x")
    .tokens({ input: 800, output: 120 })
    .text("the prompt", "the completion"),
);

await ms.shutdown(); // flush what's buffered on the way out
```

### Python

```python
from modelstat import Client, Config, LlmCall, TokenUsage

cfg = Config("msk_live_…", "raw_sdk_openai").with_remote("https://api.modelstat.ai", raw=True)
ms = Client(cfg)

# …after your real LLM call returns…
ms.record(
    LlmCall(
        "openai", "session-or-trace-id",   # provider, grouping id
        model="gpt-x",
        tokens=TokenUsage(input=800, output=120),
        prompt="the prompt",                # raw — redacted before it leaves the process
        completion="the completion",
    )
)

ms.shutdown()  # flush what's buffered on the way out
```

## Modes

| Mode | Where summarization runs | What leaves your machine | Use when |
|---|---|---|---|
| **Local daemon** *(default)* | Your machine (the daemon's local model) | Redacted abstract + metadata only | Maximum privacy; you can run a daemon on/near the host |
| **Remote** | modelstat server | Floor-redacted full turns (`raw = true`), or just the ≤320-char redacted excerpt (`raw = false`) | Serverless / can't run a local model; you accept server-side summarization |

Local-daemon mode is the default: point the SDK at a [`modelstat`](https://www.npmjs.com/package/modelstat) daemon running on loopback (`npx modelstat`), and it summarizes on your machine before anything is uploaded. Remote mode skips the local model and ships to the modelstat server directly — and even with `raw = true`, the server summarizes the turns at the ingest edge and persists only the abstract (raw is never stored).

## Privacy floor (always on)

Before any bytes leave the SDK process — in **every** mode — an in-process redaction floor scrubs:

- **Secrets**: provider API keys (Anthropic, OpenAI, Google, AWS, GitHub, Slack, Stripe, Discord…), JWTs, PEM private-key blocks, bearer tokens, `KEY=value` env secrets, and database-URL passwords.
- **PII**: email addresses and absolute home paths (`/Users/…`, `/home/…`, `C:\Users\…`).

"Raw" mode means *full turns*, not *leaked credentials* — the floor still runs. **Tool calls** ship only hashes, byte sizes, and allowlisted command verbs — never raw args, results, paths, or command text. The floor is identical across all three SDKs (a shared catalogue ported faithfully), so your privacy guarantee doesn't depend on which language you use.

## Taxonomy auto-detection (off by default)

modelstat can auto-detect a work-type *taxonomy* over your sessions, but that's tuned for interactive coding sessions — backend LLM usage usually isn't. So across all three SDKs taxonomy is **off by default**: every batch ships an explicit `auto_taxonomy: false`. Flip the config flag to opt in (`cfg.auto_taxonomy = true` in Rust/Python, `cfg.autoTaxonomy = true` in TS) and the server runs taxonomy auto-detection on your batches.

## How it stays off your hot path

`record()` does nothing but move your already-in-hand call into a bounded in-memory buffer and return. A background worker does the redaction, batching, and network I/O. If the buffer fills (a downstream stall), the newest record is dropped and a counter increments — your request is **never** blocked and memory **never** grows unbounded.

## Status

Early release. Remote mode is live end-to-end (`/v1/ingest` for excerpts, `/v1/ingest/raw` for server-side summarization). The **daemon loopback ingest** endpoint (the receiving side of local-daemon mode) is in active development — until it lands, use remote mode; the local-daemon API is stable, so your code won't change when it ships.

## License

Apache-2.0. The SDKs are intentionally self-contained and auditable — the privacy promise is something you can read.
