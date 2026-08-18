<p align="center">
  <a href="https://modelstat.ai"><img src="assets/logo.svg" alt="modelstat" height="40" /></a>
</p>

<h1 align="center">Know exactly what your AI coding spend bought.</h1>

<p align="center">
  <strong>modelstat turns the session logs Claude Code, Codex, and Cursor already write into dollar-precise spend & ROI —<br/>broken down by the real work it went to, by project, and by model. Nothing leaves your machine un-redacted.</strong>
</p>

<!-- dashboard screenshot: drop assets/activities-screenshot.png here when available -->

<p align="center">
  <a href="https://modelstat.ai"><b>modelstat.ai</b></a>
  · <a href="https://modelstat.ai/install">Install</a>
  · <a href="https://modelstat.ai/integrations">Integrations</a>
  · <a href="https://mcp.modelstat.ai">MCP server</a>
  · <a href="https://modelstat.ai/guides">Guides</a>
</p>

<p align="center">
  <a href="https://github.com/modelstat/modelstat/releases/latest"><img src="https://img.shields.io/github/v/release/modelstat/modelstat?label=daemon" alt="latest daemon release" /></a>
  <img src="https://img.shields.io/badge/privacy-redaction_runs_on_device-000000" alt="Privacy: redaction runs on device" />
</p>

---

## Install

One command installs the daemon, pairs this machine, and wires the modelstat MCP into every AI tool you have (Claude Code, Claude Desktop, Cursor, Codex, …). Paste it into a terminal — or into Claude Code / Codex / Cursor and let it run.

```bash
curl -fsSL https://modelstat.ai/install.sh | sh
```

**Windows** (PowerShell): `irm https://modelstat.ai/install.ps1 | iex`

The installer downloads a small static binary, verifies its SHA-256 checksum, pairs this machine, and installs a background service. **No Node, no Python, no package manager** — the binaries are self-contained. Re-run it any time to upgrade (auto-update is on by default).

That's it. Open your dashboard at **[modelstat.ai/dashboard](https://modelstat.ai/dashboard)**. The daemon runs on macOS 13+, every glibc Linux distro, and Windows 10/11. Full options (modes, flags, headless installs): [daemon/docs/INSTALL.md](daemon/docs/INSTALL.md).

---

## Why modelstat

- **Real work, not just raw tokens.** After about a week of passive collection, modelstat learns your team's own vocabulary of projects and work-types — so the dashboard shows *"$133 on DevOps, $114 on testing"*, not just *"you spent $400 on Claude."* No manual tagging.
- **Your actual sessions.** It reads the logs Claude Code, Codex, Cursor, Cline, Continue, Aider, Windsurf, Zed, Copilot, and Claude Desktop already write — nothing to instrument, nothing to intercept.
- **Redaction always runs on your machine.** Every session is scrubbed for secrets and PII on *your* box — a regex floor plus an on-device NER model — before a single byte is uploaded, in **every** mode. Raw keys, passwords, and personal data never leave. You choose *where the summary is written*: on this machine, your own server, or modelstat's cloud (the default) — see [the three modes below](#what-it-does-on-your-machine). [Audit it below.](#privacy--data-handling-with-proof)

---

## What this repo is

This is the **public source** for everything that runs on your machine:

- **[`modelstat` daemon](daemon/)** — the native Rust daemon: two static binaries (the collector and the summariser engine) that watch your AI-tool log files, price them, redact client-side, and upload metadata. The collector's `modelstat mcp` subcommand is also the local Model Context Protocol server, so Claude Desktop / Claude Code / Cursor / Cline / Continue / Zed can answer *"how much did we spend on X?"* in chat.
- **[macOS menu-bar tray](apps/tray-mac/)** — native Swift status-bar app.
- **[`@modelstat/sdk`](sdks/node/)** — the backend SDK: capture the LLM calls your own services make, redacted + compacted client-side before they leave the box.

**Why it's open.** The code that reads your files should be auditable. The hosted service that aggregates your team's metadata is closed-source; everything that runs on your laptop is right here — read it, fork it, build your own binaries, or pin a commit and install from source. See [LICENSE](LICENSE).

---

## What it does on your machine

```text
    your AI coding tools                     on YOUR machine                     modelstat cloud
┌──────────────────────────┐           ┌─────────────────────────┐           ┌──────────────────────┐
│   Claude Code · Codex    │           │    modelstat daemon     │           │ analytics dashboard  │
│ Cursor · Cline · Aider   │  session  │ • parse + price turns   │ redacted  │ spend & ROI grouped  │
│ Windsurf · Zed · Copilot │ ───────▶  │ • redact (PII / keys)   │ ───────▶  │ by activity · repo · │
│ Claude Desktop · …       │   logs    │ • summarise (see modes) │   HTTPS   │ model · person —     │
│ (logs already on disk)   │           │ → tokens + abstract     │           │ the charts above     │
└──────────────────────────┘           └─────────────────────────┘           └──────────────────────┘

                      ↑ secrets & PII are stripped on your machine before anything leaves ↑
```

1. **Detects installed tools** — scans `~/.claude`, `~/.codex`, `~/.cursor`, `~/.aider`, `~/.config/continue`, and other tool-specific locations.
2. **Parses local log files** — Claude Code's JSONL logs, Cursor's SQLite DB, Codex's conversation files, and so on. The [per-tool parsers](daemon/crates/modelstat-parsers/src/) are the only code that opens these files.
3. **Redacts on-device — in every mode** — every excerpt goes through a regex pass for secrets/PII *and* an on-device NER model **before** the uploader ever sees it. This never moves off your machine, regardless of the mode below.
4. **Summarises — you choose where (at install)** — the redaction in step 3 always runs locally; only *where the summary is written* differs:
   - **Local** — a small ~2.7 GB LLM summarises each work-segment on **this machine** into a ≤240-char abstract. Raw turns stay on disk; only the abstract goes up. (The only mode that downloads the model.)
   - **Self-hosted** — your org's own OpenAI-compatible endpoint summarises the cleaned excerpts (URL + model set at install). Only the abstract goes up; no local model.
   - **Cloud** *(default)* — no local model at all; the cleaned, redacted turns are uploaded and modelstat's servers summarise them.

   Change it any time with `modelstat mode <local|self-hosted|cloud>`.
5. **Uploads what the mode implies** — per-session totals (tokens, model, cost, duration) + provenance always. In **Local**/**Self-hosted**, the redacted ≤240-char abstract. In **Cloud**, the redacted turns (so the server can summarise them). Never raw prompts, code, or secrets — redaction runs on-device first in every mode.

---

## Build from source (the trust path)

```bash
git clone https://github.com/modelstat/modelstat.git
cd modelstat/daemon && cargo build --release
```

Requirements: a stable Rust toolchain, and an account at [modelstat.ai](https://modelstat.ai) to pair with. (The SDKs build with their own toolchains — Node 20.18+/pnpm 10.7+ for `sdks/node`; Swift 5.9+ for the macOS tray.)

---

## Integrations (10 supported tools)

| Command-line + agent tools | Editor / IDE |
|---|---|
| [Claude Code](https://modelstat.ai/integrations/claude-code) | [Windsurf](https://modelstat.ai/integrations/windsurf) |
| [Cursor](https://modelstat.ai/integrations/cursor) | [Zed AI](https://modelstat.ai/integrations/zed) |
| [Codex (OpenAI CLI)](https://modelstat.ai/integrations/codex) | [GitHub Copilot](https://modelstat.ai/integrations/copilot) |
| [Cline](https://modelstat.ai/integrations/cline) | [Claude Desktop](https://modelstat.ai/integrations/claude-desktop) |
| [Continue](https://modelstat.ai/integrations/continue) | |
| [Aider](https://modelstat.ai/integrations/aider) | |

Full index: **[modelstat.ai/integrations](https://modelstat.ai/integrations)**

---

## MCP — ask any AI client about your spend

The installer wires the local MCP server (`modelstat mcp`) into every AI tool it detects — nothing to configure. No daemon on this machine? Connect the hosted server instead: add **`https://mcp.modelstat.ai`** to any MCP-compatible client and sign in via the browser on first use.

> *"How much did my team spend on Claude Code last week?"*
> *"Which project is driving my Cursor cost?"*
> *"Recommend a model for a code-review task — based on what worked for us before."*

No in-band auth — the local server reuses the token the daemon already wrote on this machine. Details: **[mcp.modelstat.ai](https://mcp.modelstat.ai)** · source in [`daemon/crates/modelstat-mcp/`](daemon/crates/modelstat-mcp/).

---

## Privacy & data handling (with proof)

You can verify, from this repository alone, exactly what does and doesn't leave your machine.

**The boundary.** Exactly one crate talks to our ingest endpoints: **[`modelstat-ingest`](daemon/crates/modelstat-ingest/)** (`DeviceApi::upload_batch`). It POSTs to **`/v1/ingest`** in **Local** and **Self-hosted** modes (pre-summarised abstracts) or **`/v1/ingest/raw`** in **Cloud** mode (the redacted turns the server summarises). There is no other outbound data channel. The wire-format types live in **[`daemon/crates/modelstat-wire/src/schema.rs`](daemon/crates/modelstat-wire/src/schema.rs)** — if a field isn't there, the uploader literally cannot send it.

**What we receive — always:** model/provider/tool, per-class token counts, the cost we computed, scrubbed `cwd`/git metadata, filenames (paths scrubbed), redaction *counts* (never the matched text), and a provenance stamp. **Plus, depending on your mode:** a ≤240-char on-device abstract (**Local** / **Self-hosted**), or the redacted conversation turns themselves (**Cloud** — that is exactly what our servers summarise for you).

**What we never receive:** your raw prompts, your code, or any API key / token / password. Redaction runs **on your machine in every mode** before anything leaves it — Cloud mode's uploaded turns are redacted turns, not raw ones, and Self-hosted mode runs the same on-device scrub before excerpts reach your org's endpoint. Text fields are size-capped by Zod; the `paranoid` policy drops entire `stdout`/`stderr`/`tool_output`/`raw_text` blobs before upload.

**Defence-in-depth**, every byte crossing the boundary passes through: parser scoping → secrets regex (Anthropic/OpenAI/Google/AWS/GitHub/Slack/Stripe keys, JWTs, PEM blocks, bearer headers, DB passwords) → PII regex (emails, public IPs, URL creds, home paths) → a Shannon-entropy catcher for unknown key shapes → on-device NER → wire-schema byte caps → provenance stamp. The regex + entropy floor lives in [`daemon/crates/modelstat-redact/`](daemon/crates/modelstat-redact/); the caps in [`daemon/crates/modelstat-wire/src/caps.rs`](daemon/crates/modelstat-wire/src/caps.rs). In **Cloud** mode the daemon is **fail-closed** on the NER pass: if that on-device model can't load, it will NOT ship raw turns with regex-only redaction — it falls back to local extractive abstracts instead (no raw egress).

**Audit it yourself** — three places tell the whole story:

1. [`daemon/crates/modelstat-wire/src/schema.rs`](daemon/crates/modelstat-wire/src/schema.rs) — every type that can be uploaded.
2. [`daemon/crates/modelstat-redact/`](daemon/crates/modelstat-redact/) — the redaction passes + patterns.
3. [`daemon/crates/modelstat-ingest/`](daemon/crates/modelstat-ingest/) — the single upload path to our server.

`modelstat status` prints, locally, the same token + redaction counters the server sees. Security disclosures: [SECURITY.md](SECURITY.md) · `security@modelstat.ai`.

---

## Repo layout

```
modelstat/
├── daemon/              The shipping daemon (Rust)
│   ├── crates/          collector CLI + daemon, summariser engine, MCP server,
│   │                    parsers, redaction, wire schema, updater
│   ├── scripts/         install.sh / install.ps1 (served at modelstat.ai/install.{sh,ps1})
│   └── docs/            INSTALL.md and friends
├── apps/
│   ├── tray-mac/        macOS menu-bar app (Swift)
│   └── daemon/          retired TypeScript daemon (no longer shipped)
├── packages/            retired TypeScript packages — superseded by daemon/crates
├── sdks/                Backend SDKs (Node / Python / Rust) — current
├── prices/              Provider + model price tables
└── .github/workflows/   CI + daemon release builds + SDK publishing
```

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version:

- **Bug reports** → [GitHub Issues](https://github.com/modelstat/modelstat/issues) (email `security@modelstat.ai` if sensitive).
- **New tool integration** → open an issue first to discuss the parser shape; parsers live in [`daemon/crates/modelstat-parsers/src/`](daemon/crates/modelstat-parsers/src/).

---

## About modelstat

**modelstat** is the cross-tool AI spend analytics layer for engineering teams — the questions your finance team actually asks: *How much did we spend on project X? Which work-type eats our budget? Personal vs company accounts on the same machine? Opus vs GPT-5 vs Sonnet on the same kind of task?*

- **Website**: [modelstat.ai](https://modelstat.ai)
- **Docs for AI agents**: [modelstat.ai/llms-full.txt](https://modelstat.ai/llms-full.txt)
- **Changelog**: [modelstat.ai/changelog](https://modelstat.ai/changelog)
- **Support**: [hello@modelstat.ai](mailto:hello@modelstat.ai) · **Security**: [security@modelstat.ai](mailto:security@modelstat.ai)

---

<p align="center">
  <sub>Built for teams who'd rather know where their AI spend actually goes — without giving up the source.</sub>
</p>
