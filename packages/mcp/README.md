# @modelstat/mcp

Ask any MCP-compatible AI tool — Claude Desktop, Claude Code, Cursor, Cline, Continue, Zed — about your token spend directly in the chat.

- "How much did I spend on Cursor this week?"
- "Which project is driving my Claude Code cost?"
- "Show me recent sessions over $5."
- "Is my modelstat agent healthy?"

Uses the bearer token [`npx modelstat@latest`](https://modelstat.ai/install) already wrote to `~/.config/modelstat/state.json` — no separate auth.

## Install

```bash
# Works inline — no global install needed.
npx -y @modelstat/mcp --help

# Or pin globally:
npm install -g @modelstat/mcp
modelstat-mcp
```

## Wire it up

### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows, unsupported at the moment):

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

All tools are **read-only** except `assign_session`.

| Tool | Purpose |
|---|---|
| `usage_overview` | Spend/usage headline: cost, list price, savings, tokens, sessions. |
| `usage_explore` | The charting workhorse — group/stack by `day`, `hour`, `model`, `tool`, `provider`, `session`; metrics from cost to per-class tokens (`tokens_input`, `tokens_cache_read`, …); filter by providers/models/tools/session_ids. |
| `list_sessions` | Recent sessions with cost + tokens (cursor-paginated). |
| `session_detail` | One session's token breakdown + segments (redacted abstracts, tags). |
| `sessions_usage` | Combined usage for an explicit session-id set — e.g. one Claude Code conversation across all its compactions/resumes. |
| `assign_session` | MUTATING: reassign a session's owner. |

`range` accepts: `today`, `7d`, `30d`, `90d`, `mtd`, `ytd` — or pass explicit RFC3339 `from`/`to`. Omit both for all-time.

Prefer remote? The same tools are served over streamable HTTP at `https://modelstat.ai/mcp` — auth with `Authorization: Bearer $(npx -y modelstat@latest token)`. Claude Code users: the [modelstat plugin](https://modelstat.ai/dashboard/mcp) bundles this server and adds the `/stat` charts command.

Your MCP client may see additional tools beyond the ones listed above — the live catalog comes from the modelstat backend, and we add new query tools server-side. Ask your client to list available tools to see what's actually exposed for your account.

## Auth & privacy

The MCP server reads the bearer token that `npx modelstat@latest` stored locally. It never transmits that token anywhere except directly to the modelstat API (default `https://modelstat.ai`). Prompts, responses, and file contents never touch this process.

Override the API endpoint with `MODELSTAT_API_URL` (for self-hosted / dev). Override the state dir with `MODELSTAT_STATE_DIR`.

## Troubleshooting

- **`modelstat is not paired on this machine`** — run `npx modelstat@latest` first.
- **401 responses** — the bearer expired. Re-run `npx modelstat@latest`.
- **No data yet** — the agent uploads within a few seconds of your first AI-tool session. Check `npx modelstat@latest status`.

## License

MIT.
