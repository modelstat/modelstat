# @modelstat/mcp

Ask any MCP-compatible AI tool — Claude Desktop, Claude Code, Cursor, Cline, Continue, Zed — about your token spend directly in the chat. The same numbers you see on the [modelstat dashboard](https://modelstat.ai/dashboard), answered inline:

- "How much did I spend on Cursor this week?"
- "Total $ I spent debugging the acme project."
- "Show me recent sessions over $5."
- "What's this session costing so far?"

<!-- dashboard screenshot: drop assets/activities-screenshot.png here when available -->

**Where the data comes from** — the MCP queries the right-hand box; the rest runs on your machine:

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

**No daemon required.** If the [modelstat daemon](https://modelstat.ai/install) is installed, the MCP reuses its device token automatically (fast path). If not, the first tool call opens a browser tab to connect this MCP to your account (one-time), then remembers it in `~/.modelstat/mcp-auth.json`.

## Install

```bash
# Works inline — no global install needed.
npx -y @modelstat/mcp --help

# Or pin globally:
npm install -g @modelstat/mcp
modelstat-mcp
```

## Wire it up

**One command wires every tool you have** — Claude Code, Claude Desktop, Cursor, VS Code (+ Insiders / VSCodium), Windsurf, Zed, Codex, Gemini CLI:

```bash
npx -y @modelstat/mcp wire
```

It writes the `modelstat` server into each detected tool's config — non-destructive (your other servers are untouched) and safe to re-run. Restart any open tool afterwards. (The [modelstat installer](https://modelstat.ai/install) runs this for you.) Prefer to configure one tool by hand? The recipes are below.

### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS), `~/.config/Claude/claude_desktop_config.json` (Linux), or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```jsonc
{
  "mcpServers": {
    "modelstat": {
      "command": "npx",
      "args": ["-y", "@modelstat/mcp"]
    }
  }
}
```

Restart Claude Desktop. You'll see a 🔌 for the modelstat tools.

### Claude Code

```bash
claude mcp add modelstat -- npx -y @modelstat/mcp
```

### Cursor

Settings → Cursor Settings → MCP → Add new MCP server:

- Name: `modelstat`
- Command: `npx`
- Args: `-y @modelstat/mcp`

### Cline / Roo

Settings → MCP Servers → Edit JSON:

```json
{
  "mcpServers": {
    "modelstat": { "command": "npx", "args": ["-y", "@modelstat/mcp"] }
  }
}
```

### Continue.dev

In `~/.continue/config.yaml`:

```yaml
mcpServers:
  - name: modelstat
    command: npx
    args: ["-y", "@modelstat/mcp"]
```

## Tools

The pattern: **resolve names → ids with the `find_*` tools, then pass the ids as filters to `explore`/`sessions`.** All tools are **read-only** except `assign_session`.

| Tool | Purpose |
|---|---|
| `overview` | Spend/usage headline: effective cost, list price, savings, tokens, sessions, taxonomy roots. |
| `explore` | The analytics workhorse — group/stack by `day`, `hour`, `model`, `tool`, `provider`, `session`, `identity`, `taxonomy`; metrics from cost to per-class tokens; filter by providers/models/tools/identities/session_ids and a `taxonomy` AND-of-OR filter (`[[projId],[debugId]]` = tagged BOTH). |
| `sessions` | Search/list sessions — filter by taxonomy groups, identity, device, free-text `q`, range (cursor-paginated). |
| `session_insights` | Live per-session insights for the CURRENT session: tokens, effective $, taxonomy detected, + a `ready`/`analyzing`/`not_ingested` status. `eager:true` force-scans the session locally first; then re-poll while `analyzing`. In [MCP Apps](https://github.com/modelcontextprotocol/ext-apps) hosts (Claude Desktop/web) this renders as an inline card; elsewhere it's text + structured data. |
| `find_taxonomy` | Resolve taxonomy node names → ids (optional `root_key`). |
| `find_projects` | List/search projects (the `workstreams` root) → node ids, with spend. |
| `find_people` | Search identities → ids for the `identities` filter. |
| `assign_session` | MUTATING: reassign a session's owner. |

`range` accepts: `today`, `7d`, `30d`, `90d`, `mtd`, `ytd` — or pass explicit RFC3339 `from`/`to`. Omit both for all-time.

Prefer remote? The same tools are served over streamable HTTP at `https://mcp.modelstat.ai` — auth with `Authorization: Bearer $(npx -y modelstat@latest token)`. Claude Code users: the [modelstat plugin](https://modelstat.ai/dashboard/mcp) bundles this server and adds the `/stat` charts command.

Your MCP client may see additional tools beyond the ones listed above — the live catalog comes from the modelstat backend, and the bridge forwards it verbatim. Ask your client to list available tools to see what's actually exposed for your account.

## Auth & privacy

Auth resolution, most-trusted first: (1) `MODELSTAT_TOKEN` env, (2) a running daemon's token (`modelstat token`), (3) the daemon identity at `~/.modelstat/identity.json`, (4) our own `~/.modelstat/mcp-auth.json` written by the browser claim. With none of those, the first tool call runs the browser claim. The bearer is sent only to the modelstat API (default `https://modelstat.ai`); prompts, responses, and file contents never touch this process.

Override the API endpoint with `MODELSTAT_API_URL` (self-hosted / dev), the bearer with `MODELSTAT_TOKEN`, or the daemon home with `MODELSTAT_HOME`. Set `MODELSTAT_MCP_BROWSER_AUTH=0` to disable the browser claim (or `=1` to force it in a non-TTY context).

## Troubleshooting

- **`modelstat isn't connected yet`** — claim it via the browser tab the first tool call opens, or install the daemon with `npx modelstat@latest`.
- **401 responses** — the token expired. Re-run `npx modelstat@latest`, or just retry to re-claim.
- **No data yet** — usage uploads within a few seconds of your first AI-tool session. Check `npx modelstat@latest status`.
- **No browser opened in a headless host** — set `MODELSTAT_TOKEN=$(modelstat token)` (or `MODELSTAT_MCP_BROWSER_AUTH=1` to force the prompt).

## License

MIT.
