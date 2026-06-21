# modelstat for Claude Code

Charts of your AI usage, inside your session.

```bash
claude plugin marketplace add modelstat/modelstat
claude plugin install modelstat@modelstat
```

| Try | Get |
|---|---|
| `/stat` | 30-day dashboard: spend, daily trend, top models |
| `/stat session` | the session you're in — eagerly scanned + merged across compactions: tokens, $ assigned, work-types detected |
| `/stat models 7d` | model leaderboard |
| `/stat $ debugging acme-dash` | resolve names → ids, then filter — anything `explore` can answer |
| `/stat cache hit rate by day, 2 weeks` | per-class token math over any range |

Bundles the [`@modelstat/mcp`](https://www.npmjs.com/package/@modelstat/mcp) server, so the tools also answer plain questions ("how much did Opus cost me this month?") without `/stat`.

Works with a paired [modelstat](https://modelstat.ai/install) daemon (`npx modelstat@latest`) — or, with no daemon, the MCP connects itself via your browser on first use.
