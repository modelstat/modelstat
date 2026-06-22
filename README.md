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
  · <a href="https://modelstat.ai/mcp">MCP server</a>
  · <a href="https://modelstat.ai/guides">Guides</a>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/modelstat"><img src="https://img.shields.io/npm/v/modelstat?label=modelstat" alt="npm modelstat" /></a>
  <a href="https://www.npmjs.com/package/@modelstat/mcp"><img src="https://img.shields.io/npm/v/@modelstat/mcp?label=%40modelstat%2Fmcp" alt="npm @modelstat/mcp" /></a>
  <img src="https://img.shields.io/badge/privacy-prompts_stay_on_device-000000" alt="Privacy: prompts stay on device" />
</p>

---

## Install

One command pairs your machine, downloads the on-device model, and installs a background service. Re-run it any time to upgrade.

```bash
npx modelstat@latest
```

Prefer curl?

```bash
curl -fsSL https://install.modelstat.ai | sh
```

That's it. Open your dashboard at **[modelstat.ai/dashboard](https://modelstat.ai/dashboard)**. Works on macOS and Linux with Node 20+.

---

## Why modelstat

- **Real work, not just raw tokens.** After about a week of passive collection, modelstat learns your team's own vocabulary of projects and work-types — so the dashboard shows *"$133 on DevOps, $114 on testing"*, not just *"you spent $400 on Claude."* No manual tagging.
- **Your actual sessions.** It reads the logs Claude Code, Codex, Cursor, Cline, Continue, Aider, Windsurf, Zed, Copilot, and Claude Desktop already write — nothing to instrument, nothing to intercept.
- **Local-first by construction.** A small model on *your* machine summarizes and redacts every session before anything is uploaded. Raw prompts, code, and secrets never leave the box — only token counts, cost, and a short scrubbed abstract. [Audit it below.](#privacy--data-handling-with-proof)

---

## What this repo is

This is the **public source** for everything that runs on your machine:

- **[`modelstat`](apps/daemon/)** — the Node daemon that watches your AI-tool log files, prices them, redacts client-side, and uploads metadata.
- **[`@modelstat/mcp`](packages/mcp/)** — a Model Context Protocol server so Claude Desktop / Claude Code / Cursor / Cline / Continue / Zed can answer *"how much did we spend on X?"* in chat.
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
│ Claude Desktop · …       │   logs    │ • summarize (local LLM) │   HTTPS   │ model · person —     │
│ (logs already on disk)   │           │ → tokens + abstract     │           │ the charts above     │
└──────────────────────────┘           └─────────────────────────┘           └──────────────────────┘

                      ↑ raw prompts, code & secrets never leave your machine ↑
```

1. **Detects installed tools** — scans `~/.claude`, `~/.codex`, `~/.cursor`, `~/.aider`, `~/.config/continue`, and other tool-specific locations.
2. **Parses local log files** — Claude Code's JSONL logs, Cursor's SQLite DB, Codex's conversation files, and so on. The [per-tool parsers](packages/parsers/src/) are the only code that opens these files.
3. **Redacts on-device** — every excerpt goes through a regex pass for secrets/PII *and* (optionally) a local NER model **before** the uploader ever sees it.
4. **Summarises on-device** — a small local LLM compresses each work-segment into a single ≤240-char abstract. The raw turns stay on disk; only the abstract goes up.
5. **Uploads metadata only** — per-session totals (tokens, model, cost, duration) + the redacted abstract + provenance describing what was redacted.

---

## Build from source (the trust path)

```bash
git clone https://github.com/modelstat/modelstat.git
cd modelstat && pnpm install && pnpm build
```

Requirements: Node 20.18+, pnpm 10.7+, Swift 5.9+ (tray only), and an account at [modelstat.ai](https://modelstat.ai) to pair with.

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

Once paired, any MCP-compatible client can query your spend in natural language:

```jsonc
// ~/Library/Application Support/Claude/claude_desktop_config.json (or ~/.cursor/mcp.json, etc.)
{ "mcpServers": { "modelstat": { "command": "npx", "args": ["-y", "@modelstat/mcp"] } } }
```

> *"How much did my team spend on Claude Code last week?"*
> *"Which project is driving my Cursor cost?"*
> *"Recommend a model for a code-review task — based on what worked for us before."*

No in-band auth — the server reuses the token `npx modelstat@latest` already wrote locally. Details: **[modelstat.ai/mcp](https://modelstat.ai/mcp)** · source in [`packages/mcp/`](packages/mcp/).

---

## Privacy & data handling (with proof)

You can verify, from this repository alone, exactly what does and doesn't leave your machine.

**The boundary.** Exactly one function talks to our server: **[`IngestClient.upload()`](packages/daemon-core/src/http/index.ts) → POST `/v1/ingest`**. There is no other outbound channel. The wire-format types live in **[`packages/core/src/schemas.ts`](packages/core/src/schemas.ts)** as Zod schemas — if a field isn't there, the uploader literally cannot send it.

**What we receive:** model/provider/tool, per-class token counts, the cost we computed, scrubbed `cwd`/git metadata, filenames (paths scrubbed), an optional ≤320-char pre-redacted excerpt, a ≤240-char on-device abstract, redaction *counts* (never the matched text), and a provenance stamp.

**What we never receive:** your raw prompts, your code, or any API key / token / password. Text fields are size-capped by Zod and pass through redaction first; the `paranoid` policy drops entire `stdout`/`stderr`/`tool_output`/`raw_text` blobs before upload.

**Defence-in-depth**, every byte crossing the boundary passes through: parser scoping → secrets regex (Anthropic/OpenAI/Google/AWS/GitHub/Slack/Stripe keys, JWTs, PEM blocks, bearer headers, DB passwords) → PII regex (emails, public IPs, URL creds, home paths) → a Shannon-entropy catcher for unknown key shapes → optional on-device NER → Zod length caps → policy gate → provenance stamp. All in [`packages/core/src/redact.ts`](packages/core/src/redact.ts).

**Audit it yourself** — three files tell the whole story:

1. [`packages/core/src/schemas.ts`](packages/core/src/schemas.ts) — every type that can be uploaded.
2. [`packages/core/src/redact.ts`](packages/core/src/redact.ts) — the redaction policies + patterns.
3. [`packages/daemon-core/src/http/index.ts`](packages/daemon-core/src/http/index.ts) — the single `fetch()` to our server.

`npx modelstat@latest status` prints, locally, the same token + redaction counters the server sees. Security disclosures: [SECURITY.md](SECURITY.md) · `security@modelstat.ai`.

---

## Repo layout

```
modelstat/
├── apps/
│   ├── daemon/          Node daemon CLI (modelstat)
│   └── tray-mac/        macOS menu-bar app (Swift)
├── packages/
│   ├── daemon-core/     Shared pipeline / queue / HTTP for the daemon
│   ├── core/            Shared enums + Zod schemas (wire format lives here)
│   ├── mcp/             Model Context Protocol server (@modelstat/mcp)
│   ├── parsers/         Per-tool log parsers (Claude Code / Codex / Cursor / ...)
│   └── pricing/         Provider + model price tables
├── sdks/                Backend SDKs (Node / Python / Rust)
└── .github/workflows/   npm + SDK publishing
```

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version:

- **Bug reports** → [GitHub Issues](https://github.com/modelstat/modelstat/issues) (email `security@modelstat.ai` if sensitive).
- **New tool integration** → open an issue first to discuss the parser shape; parsers live in [`packages/parsers/src/`](packages/parsers/src/).

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
