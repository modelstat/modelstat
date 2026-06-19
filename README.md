<p align="center">
  <a href="https://modelstat.ai"><img src="assets/logo.svg" alt="modelstat" height="40" /></a>
</p>

<h1 align="center">modelstat — daemon source</h1>

<p align="center">
  <strong>Track every AI token your team spends — across Claude Code, Cursor, Codex, Cline, Continue, Aider, Copilot, Claude Desktop, and web chat.</strong>
</p>

<p align="center">
  <a href="https://modelstat.ai"><b>modelstat.ai</b></a>
  · <a href="https://modelstat.ai/install">Install</a>
  · <a href="https://modelstat.ai/integrations">Integrations</a>
  · <a href="https://modelstat.ai/mcp">MCP server</a>
  · <a href="https://modelstat.ai/pricing">Pricing</a>
  · <a href="https://modelstat.ai/guides">Guides</a>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/modelstat"><img src="https://img.shields.io/npm/v/modelstat?label=modelstat" alt="npm modelstat" /></a>
  <a href="https://www.npmjs.com/package/@modelstat/mcp"><img src="https://img.shields.io/npm/v/@modelstat/mcp?label=%40modelstat%2Fmcp" alt="npm @modelstat/mcp" /></a>
  <img src="https://img.shields.io/badge/privacy-prompts_stay_on_device-000000" alt="Privacy: prompts stay on device" />
</p>

---

## What this repo is

This is the **public source** for everything that runs on your machine:

- **[`modelstat`](apps/daemon/)** — the Node daemon that watches Claude Code / Codex / Cursor / Aider / Cline / Continue / Windsurf / Zed / Copilot log files, prices them, redacts client-side, and uploads metadata.
- **[`@modelstat/mcp`](packages/mcp/)** — a Model Context Protocol server so Claude Desktop / Claude Code / Cursor / Cline / Continue / Zed can answer "how much did we spend on X?" in chat.
- **[macOS menu-bar tray](apps/tray-mac/)** — native Swift status-bar app.
- **[`@modelstat/daemon-sdk`](packages/daemon-sdk/)** — client-side redaction + compaction primitives. The boundary between your machine and our server.

**Why it's open.** The code that watches your files needs to be auditable. The hosted service that aggregates your team's metadata is closed-source; everything that runs on your laptop is right here. You can read it, fork it, build your own binaries, or pin a specific commit to install from source.

See [LICENSE](LICENSE) for usage terms.

---

## Getting modelstat (the fast path)

You don't need this repo to *use* modelstat. One command installs the published daemon, downloads the on-device summariser model, pairs your machine, and installs the background service:

```bash
npx modelstat@latest
```

That's it. Re-running the same command upgrades you to the newest version (postinstall stops the old service, swaps the bundle, restarts it). Works on any machine with Node 20+.

Diagnostics + maintenance:

```bash
npx modelstat@latest status                # show pairing + service state
npx modelstat@latest stats                 # live device summary
npx modelstat@latest remove                # stop and uninstall the background service
```

---

## Building from source (the trust path)

If you'd rather audit + build it yourself:

```bash
git clone https://github.com/modelstat/modelstat.git
cd modelstat
pnpm install
pnpm build
```

Individual components:

```bash
# Node daemon
pnpm --filter modelstat build
node apps/daemon/dist/cli.mjs

# MCP server
pnpm --filter @modelstat/mcp build
node packages/mcp/dist/index.mjs

# macOS menu-bar tray (Swift)
pnpm tray-mac:build
./apps/tray-mac/.build/release/modelstat-tray
```

**Requirements**

- Node 20.18+ and pnpm 10.7+
- Swift 5.9+ (only for the tray app)
- An account at [modelstat.ai](https://modelstat.ai) to pair with (free tier: 200M tokens / month, no card)

---

## What it does on your machine

1. **Detects installed tools** — scans `~/.claude`, `~/.codex`, `~/.cursor`, `~/.aider`, `~/.config/continue`, and other tool-specific locations to find which AI tools you have installed.
2. **Parses local log files** — Claude Code's JSONL conversation logs, Cursor's SQLite DB, Codex's conversation files, Aider's chat history, and so on. The [per-tool parsers](packages/parsers/src/) are the only code that ever opens these files.
3. **Redacts on-device** — every excerpt goes through a regex pass for secrets/PII *and* (optionally) a local NER model for names/orgs/locations **before** anything is handed to the uploader.
4. **Summarises on-device** — a small local LLM compresses each work-segment into a single ≤240-char abstract. The raw turn contents stay on disk; only the abstract goes up.
5. **Uploads metadata only** — per-session totals (tokens, model, cost, duration) + the redacted abstract + provenance metadata describing what was redacted. See the [Privacy section](#privacy--data-handling-with-proof) below.
6. **MCP server** — wraps the read-only spend queries as MCP tools so any MCP-compatible client (Claude Desktop / Claude Code / Cursor / Cline / Continue / Zed) can ask about your spend in chat.

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

Once installed, any MCP-compatible client (Claude Desktop / Claude Code / Cursor / Cline / Continue / Zed) can query your spend in natural language.

```jsonc
// ~/Library/Application Support/Claude/claude_desktop_config.json
// (or ~/.cursor/mcp.json, etc.)
{
  "mcpServers": {
    "modelstat": {
      "command": "npx",
      "args": ["-y", "@modelstat/mcp"]
    }
  }
}
```

Then in any chat:

> **"How much did my team spend on Claude Code last week?"**
> **"Which project is driving my Cursor cost?"**
> **"Recommend a model for a code-review task under $0.50 — based on what worked for us before."**

Core read tools:

| Tool | What it does |
|---|---|
| `get_spend_summary` | Total $ + tokens for a range, grouped by tool and model |
| `get_spend_by_project` | Spend grouped by auto-detected project (repo) |
| `get_spend_by_tool` | Spend grouped by AI tool |
| `list_recent_sessions` | Recent sessions with cost — spot outliers |
| `get_device_status` | Pairing + last-heartbeat check |
| `recommend_model` | Cost-optimal model pick backed by YOUR historical data |

No in-band auth — the MCP server reads the bearer token that `npx modelstat@latest` already wrote locally. See [`packages/mcp/src/`](packages/mcp/src/) for the full source.

---

## Privacy & data handling (with proof)

modelstat is built so a developer reading their own logs (or a security reviewer) can verify, from this repository alone, exactly what does and doesn't leave their machine. Every claim below points to the file and lines in this repo that back it up.

### The boundary

Exactly one function on this side talks to our server: **[`IngestClient.upload()`](packages/daemon-core/src/http/index.ts) → POST `/v1/ingest`**. There is no other outbound channel from the daemon. Audit that one entry point and you've audited everything you upload.

The wire-format types live in **[`packages/core/src/schemas.ts`](packages/core/src/schemas.ts)** — Zod schemas, so the upload payload is statically constrained to the shapes documented there. If a field isn't in those schemas, the uploader literally cannot send it.

### What we receive

| Field | What it is | Where it's defined |
|---|---|---|
| `model`, `provider`, `tool` | Which model/provider/tool the turn used | [`schemas.ts` → RawEvent](packages/core/src/schemas.ts) |
| `tokens` | Per-class token counts (input/output/cache/reasoning) | RawEvent |
| `cost_usd` | What we priced the turn at, using the public [`prices/`](packages/pricing/) rate cards | RawEvent |
| `cwd`, `git.{host,slug,branch}` | Working directory + repo metadata (paths inside `/Users/` are scrubbed) | RawEvent |
| `files_touched` | Filenames mentioned in the turn (paths scrubbed) | RawEvent |
| `content_excerpt` | An optional ≤320-char excerpt the parser may include — **already passed through the redaction pipeline** | RawEvent |
| `abstract` | One sentence (≤240 chars), generated **on-device** by a local LLM from already-redacted content | [`schemas.ts` → Segment](packages/core/src/schemas.ts), cap in [`pipeline/prompts.ts`](packages/daemon-core/src/pipeline/prompts.ts) |
| `redaction` | Per-segment **counts** of what was redacted (`{secret: 3, email: 1, …}`) — never the matched text | Segment |
| `tags` | Structured labels (e.g. `[Mood: focused] [Mind: debugging]`) from the on-device cognition pass — closed vocabulary, no freeform text | [`pipeline/cognition.ts`](packages/daemon-core/src/pipeline/cognition.ts) |
| `processing` | Provenance: `{ redacted_by, redaction_policy, redaction_policy_version, redactions_applied, original_size_bytes, uploaded_size_bytes }` | [`daemon-sdk/src/redact.ts → processingFor()`](packages/daemon-sdk/src/redact.ts) |

### What we never receive

| Claim | Why it's true |
|---|---|
| **Your raw prompts** | The parsers never copy prompt text into the wire-format objects. The only text fields that can ship are `content_excerpt` (capped at 320 chars, pre-redacted) and `abstract` (capped at 240 chars, generated **after** redaction by an LLM running on your machine). Both are bounded by Zod max-length constraints in [`packages/core/src/schemas.ts`](packages/core/src/schemas.ts). |
| **Your code** | Same path — code fragments that appear in tool inputs/outputs go through the same redaction layer (which catches API keys, paths, secrets) and are size-capped. The `paranoid` policy ([line 176-179 of `daemon-sdk/src/redact.ts`](packages/daemon-sdk/src/redact.ts)) drops every `stdout`/`stderr`/`tool_output`/`raw_text` blob entirely before upload. |
| **API keys, tokens, passwords** | Stripped pre-upload by ~15 explicit regex patterns ([`packages/daemon-sdk/src/redact.ts` lines 48-71](packages/daemon-sdk/src/redact.ts)) plus a Shannon-entropy catcher for unknown high-entropy strings ([`packages/core/src/redact.ts` lines 41-82](packages/core/src/redact.ts)). |

### Defence-in-depth layers

Every byte that crosses the boundary has been through several independent filters:

| Layer | What it catches | Where |
|---|---|---|
| **Parser scope** | Parsers only read fields they declare an interest in; they don't grab whole files. | [`packages/parsers/src/`](packages/parsers/src/) |
| **Regex pass (secrets)** | Anthropic / OpenAI / Google / AWS / GitHub / Slack / Stripe keys, JWTs, PEM blocks, `Bearer` headers, DB URLs with passwords, `ds_live_*` device tokens | [`packages/daemon-sdk/src/redact.ts` lines 48-71](packages/daemon-sdk/src/redact.ts) |
| **Regex pass (PII)** | Emails, phone numbers, **public** IPv4 (private/loopback skipped on purpose), URL credentials (`https://user:pass@…`), absolute home paths (`/Users/`, `/home/`, `C:\Users\`) | [`packages/daemon-sdk/src/redact.ts` lines 79-96](packages/daemon-sdk/src/redact.ts) |
| **Entropy catcher** | Any ≥32-char token with Shannon entropy ≥ 3.6 bits/char (unknown API-key shapes) | [`packages/core/src/redact.ts` lines 41-82](packages/core/src/redact.ts) |
| **On-device NER** | Person names, organisation names, locations — via a quantised model running locally through `@huggingface/transformers` (WebGPU in the browser, CPU in Node). Optional peer dep; if missing, the regex layer stands alone. | [`packages/daemon-core/src/redact/privacy-filter.ts`](packages/daemon-core/src/redact/privacy-filter.ts) |
| **Length caps** | Hard upper bounds on every textual wire field, enforced by Zod | [`packages/core/src/schemas.ts`](packages/core/src/schemas.ts) |
| **Policy gate** | The `paranoid` policy drops the entire `stdout`/`stderr`/`tool_output`/`raw_text` family of fields rather than redact them | [`packages/daemon-sdk/src/redact.ts` lines 176-179](packages/daemon-sdk/src/redact.ts) |
| **Provenance stamp** | Every upload carries which policy ran, which version, how many redactions were applied, bytes-before / bytes-after — visible to you in the dashboard | [`packages/daemon-sdk/src/redact.ts → processingFor()`](packages/daemon-sdk/src/redact.ts) |

### Policies

Four built-in policies, defined in [`packages/daemon-sdk/src/redact.ts` lines 19-26](packages/daemon-sdk/src/redact.ts):

| Policy | Behaviour |
|---|---|
| `none` | Pass-through. Opt-in only; the default is never `none`. |
| `secrets-only` | API keys, JWTs, PEM blocks. Leaves PII alone. |
| `strict-pii-v2` (default) | Secrets + PII + on-device NER if available. |
| `paranoid` | `strict-pii-v2` + drop entire stdout/stderr/output blob fields. |

### Audit it yourself

If you want to verify the wire format end-to-end, the three things to read are:

1. **[`packages/core/src/schemas.ts`](packages/core/src/schemas.ts)** — every type that can be uploaded. If a field isn't here, the uploader can't ship it.
2. **[`packages/daemon-sdk/src/redact.ts`](packages/daemon-sdk/src/redact.ts)** — the redaction policies + patterns.
3. **[`packages/daemon-core/src/http/index.ts`](packages/daemon-core/src/http/index.ts)** — the single function that calls `fetch()` to our server. Everything that leaves your machine passes through here.

You can also run `npx modelstat@latest stats` to print, locally, a summary of what's been uploaded — token counts and redaction counters straight from the same provenance metadata the server sees.

Security disclosures: please read [SECURITY.md](SECURITY.md) and email `security@modelstat.ai`.

---

## Repo layout

```
modelstat/
├── apps/
│   ├── daemon/          Node daemon CLI (modelstat)
│   └── tray-mac/           macOS menu-bar app (Swift)
├── packages/
│   ├── daemon-sdk/          Redact + compact primitives (@modelstat/daemon-sdk)
│   ├── daemon-core/     Shared pipeline / queue / HTTP for the daemon
│   ├── core/               Shared enums + Zod schemas (wire format lives here)
│   ├── mcp/                Model Context Protocol server (@modelstat/mcp)
│   ├── parsers/            Per-tool log parsers (Claude Code / Codex / Cursor / ...)
│   └── pricing/            Provider + model price tables
└── .github/workflows/      npm publishing workflow
```

---

## Tech stack (client-side)

**Node CLI** — TypeScript, `chokidar` for file watching, `undici` HTTP, `conf` for state, `sql.js` for Cursor's SQLite, [ulid](https://github.com/ulid/javascript) + UUIDv7 for IDs.

**MCP server** — `@modelcontextprotocol/sdk` over stdio.

**macOS tray** — Swift 5.9 + SwiftUI + `LaunchAgent`.

**On-device LLMs** — quantised summariser + cognition pass run via a bundled `node-llama-cpp`. Privacy-filter NER runs via `@huggingface/transformers` (CPU/ONNX in Node). All models stay on your machine; nothing is shipped to a remote inference provider from the daemon.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

The short version:

- **Bug reports**: [GitHub Issues](https://github.com/modelstat/modelstat/issues) — or email `hello@modelstat.ai` if it's security-sensitive.
- **New tool integration** (e.g. a new AI coding tool): open an issue first to discuss the parser shape. Parser additions go in [`packages/parsers/src/`](packages/parsers/src/).
- **Security disclosures**: please read [SECURITY.md](SECURITY.md) first.

---

## About modelstat

**modelstat** is the cross-tool AI spend analytics layer for engineering teams. We answer the questions your finance team actually cares about:

- How much did we spend on **project X** last quarter?
- Which **work type** (debugging vs planning vs docs) eats our budget?
- Who's spending on **personal** vs **company** accounts from the same machine?
- What is Claude Opus costing us vs GPT-5 vs Sonnet on the same kinds of tasks?

After about a week of passive collection, modelstat learns your team's own vocabulary of projects and work types automatically — no manual tagging required.

- **Website**: [modelstat.ai](https://modelstat.ai)
- **Pricing**: [modelstat.ai/pricing](https://modelstat.ai/pricing) — free tier: 200M tokens/month, no card
- **Docs for AI agents**: [modelstat.ai/llms-full.txt](https://modelstat.ai/llms-full.txt)
- **Status**: [modelstat.ai/changelog](https://modelstat.ai/changelog)
- **Support**: [hello@modelstat.ai](mailto:hello@modelstat.ai)
- **Security**: [security@modelstat.ai](mailto:security@modelstat.ai) · [SECURITY.md](SECURITY.md)

---

<p align="center">
  <sub>Built with care for teams who'd rather know where their AI spend actually goes — without giving up the source.</sub>
</p>
