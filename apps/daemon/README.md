# modelstat

> **Know exactly what your AI coding spend bought.** modelstat turns the session logs Claude Code, Codex, and Cursor already write into dollar-precise spend & ROI — broken down by the real work it went to, by project, and by model.

<!-- dashboard screenshot: drop assets/activities-screenshot.png here when available -->

**Local-first by construction.** A small model on *your* machine summarizes and redacts every session before anything is uploaded. Raw prompts, code, and secrets never leave the box — only token counts, cost, and a short scrubbed abstract. The source is auditable on [GitHub](https://github.com/modelstat/modelstat/tree/main/apps/daemon).

## How it works

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

## Install

One command pairs your machine, downloads the on-device model, and installs a background service. Re-run it any time to upgrade.

```bash
npx modelstat@latest
```

Prefer curl? Or another runner?

```bash
curl -fsSL https://install.modelstat.ai | sh   # detects pnpm / bun / npm for you
bunx modelstat@latest
pnpm dlx modelstat@latest
```

The first run downloads the on-device summariser model (~2.7 GB Qwen GGUF to `~/.modelstat/models/`), pairs the device, and installs a **launchd** user daemon on macOS (`~/Library/LaunchAgents/ai.modelstat.daemon.plist`) or a **systemd** user unit on Linux (`~/.config/systemd/user/modelstat.service`). It starts on login and watches your AI-tool logs in the background; the CLI then exits — there's no foreground process to keep open.

Requires Node 20+. macOS and Linux (x86_64, arm64). Then open **[modelstat.ai/dashboard](https://modelstat.ai/dashboard)**.

## Works with your real sessions

Claude Code · Codex · Cursor · Cline · Continue · Aider · Windsurf · Zed · GitHub Copilot · Claude Desktop. Nothing to instrument and nothing to intercept — modelstat reads the logs these tools already write. Per-tool setup: [modelstat.ai/integrations](https://modelstat.ai/integrations).

## Commands

```bash
npx modelstat@latest                     # install or upgrade. Default action.
npx modelstat@latest remove              # stop and uninstall the background service

npx modelstat@latest status              # pairing, service + live usage: sessions · tokens · cost
npx modelstat@latest jobs                # pipeline queue + recent processing ledger
npx modelstat@latest paths [--json]      # state file + log dir + API URL

npx modelstat@latest sync --session <id> # force-ingest ONE session now (warms a running daemon)
npx modelstat@latest reset               # reset cursors so the daemon re-reads everything
npx modelstat@latest watch               # foreground watcher (no service install)
npx modelstat@latest discover            # report detected tool installs + identities
modelstat statusline                     # Claude Code status line (reads its stdin JSON)
```

**Headless pairing:** `npx modelstat@latest --json --no-browser` emits one NDJSON event per line so a wrapper can drive pairing non-interactively.

## Claude Code status line

The installer auto-enables a live status line in Claude Code so every turn shows your current session's **tokens · effective $ · taxonomy**. It reads only a small local cache (`~/.modelstat/sessions/<id>.json`) — never blocks the prompt, never calls the network. It composes with (and restores) any status line you already had:

```json
{ "statusLine": { "type": "command", "command": "modelstat statusline" } }
```

Opt out at install with `MODELSTAT_NO_STATUSLINE=1`, or remove it later with `npx modelstat@latest remove`.

## MCP — ask any AI client about your spend

Pair the daemon, then add [`@modelstat/mcp`](https://www.npmjs.com/package/@modelstat/mcp) to query your own spend from inside Claude Code, Claude Desktop, Cursor, Cline, Continue, or Zed:

```bash
claude mcp add modelstat -- npx -y @modelstat/mcp
```

Full per-client docs: [modelstat.ai/mcp](https://modelstat.ai/mcp).

## Shared state across install methods

All daemon state lives in one home directory, `~/.modelstat`, identical on every OS. `identity.json` holds your device UUID + bearer token (`0600`); `state.json` holds runtime state (file cursors, …). Set `MODELSTAT_HOME` to relocate everything at once (e.g. `MODELSTAT_HOME=/opt/modelstat` for a system-wide, one-per-server install). Installing via both Homebrew and npm yields the **same binary reading the same state**, and the service deduplicates the device server-side — you won't see the same laptop twice.

## Self-host

Point the daemon at your own modelstat API instead of the hosted service:

```bash
export DAEMON_API_URL=https://your-modelstat-api.example.com
npx modelstat@latest
```

`DAEMON_API_URL` can also be set persistently via `.env` or in the systemd/launchd unit.

## Privacy

- Reads local session logs the tools already write — nothing is intercepted.
- **Uploaded:** token counts, model name, timestamps, provider-assigned session id, redactable git remote, and a redacted work-type abstract.
- **Never uploaded:** prompt text, model responses, file contents, tool-call arguments, environment variables, SSH keys, secrets.
- Redaction runs on-device; offline sessions buffer locally and upload when the network returns.

## License

Apache-2.0. Source at https://github.com/modelstat/modelstat/tree/main/apps/daemon.
