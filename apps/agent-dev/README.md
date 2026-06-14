# modelstat

> **See every AI token your team spends.** Local companion for [modelstat](https://modelstat.ai) — reads the session logs your AI coding tools already write (Claude Code, Codex, Cursor, Cline, Continue, Aider, Windsurf, Zed, Copilot, Claude Desktop), tokenises events on-device, and uploads only metadata to your modelstat dashboard.

**Your prompts never leave your machine.** The agent uploads only token counts, model ids, timestamps, and a provider-assigned session id. Source is auditable on [GitHub](https://github.com/modelstat/modelstat/tree/main/apps/agent-dev).

## Install

One command. Re-running it upgrades you to the newest published version (postinstall stops the running service, swaps the bundle, restarts it).

```bash
npx modelstat@latest
```

Same one-shot via Bun or pnpm:

```bash
bunx modelstat@latest
pnpm dlx modelstat@latest
```

The first run downloads the on-device summariser model (~2.7 GB Qwen3.5-4B GGUF to `~/.modelstat/models/`), pairs the device, and installs a **launchd user agent** on macOS (at `~/Library/LaunchAgents/ai.modelstat.agent.plist`) or a **systemd user unit** on Linux (at `~/.config/systemd/user/modelstat.service`). The daemon starts automatically on login and watches your AI-tool session logs in the background. The CLI exits cleanly — there's no foreground process to keep open.

Requires Node 20+. macOS and Linux (x86_64, arm64) supported.

## Commands

```bash
npx modelstat@latest                    # install or upgrade. Default action.
npx modelstat@latest remove             # stop and uninstall the background service
npx modelstat@latest reinstall          # alias for the default — explicit form

npx modelstat@latest status             # show pairing + service state
npx modelstat@latest stats              # live device summary: sessions · tokens · cost
npx modelstat@latest jobs               # pipeline queue + recent processing ledger
npx modelstat@latest paths [--json]     # state file + log dir + API URL

npx modelstat@latest scan               # one-shot parse + upload of local JSONL
npx modelstat@latest rescan             # wipe file cursors so next scan re-reads & re-summarises everything
npx modelstat@latest watch              # foreground watcher (no service install)
npx modelstat@latest discover           # report detected tool installs + identities
```

**Programmatic pairing** (for scripted / headless setups that pair without a browser):

```bash
npx modelstat@latest --json --no-browser
```

Emits one NDJSON event per line, so a wrapper can drive pairing non-interactively.

## Shared state across install methods

Installing via both Homebrew and npm on the same laptop produces the **same binary reading the same state file** — on macOS that's `~/Library/Preferences/modelstat-agent-dev-nodejs/config.json`. Your device UUID, bearer token, and pairing state persist across install methods — and the service deduplicates the device server-side, so you won't see the same laptop twice in the dashboard.

## MCP server

Pair the agent, then install [`@modelstat/mcp`](https://www.npmjs.com/package/@modelstat/mcp) to query your own spend from inside Claude Desktop, Cursor, Cline, Continue, or Zed:

```bash
# Claude Code
claude mcp add modelstat -- npx -y @modelstat/mcp
```

Full wire-up docs per client: https://modelstat.ai/mcp

## Self-host

To point the agent at your own modelstat API (not the hosted SaaS):

```bash
export AGENT_API_URL=https://your-modelstat-api.example.com
npx modelstat@latest
```

`AGENT_API_URL` can also be set persistently via `.env` or in the systemd/launchd unit.

## Privacy

- Agent reads local session logs written by the tools you use. Nothing is intercepted — the tools already write these files.
- Upload payload: token counts, model name, timestamps, provider-assigned session id, git remote URL (redactable), redacted work-type summary.
- Never uploaded: prompt text, model responses, file contents, tool-call arguments, environment variables, SSH keys, secrets.
- Redaction is on-device via [`@modelstat/parsers`](https://github.com/modelstat/modelstat/tree/main/packages/parsers).
- Offline mode: buffered locally, uploaded when network returns. `modelstat scan --dry-run` for local-only analytics.

## Pricing

- **Free**: 100M tokens/month, 1 device, no card.
- **Team**: $5/seat/month with 250M pooled tokens; overage at $25/billion.
- **Enterprise**: SSO/SCIM, on-prem ingest, SLAs — contact `hello@modelstat.ai`.

## License

Apache-2.0. Source at https://github.com/modelstat/modelstat/tree/main/apps/agent-dev.
