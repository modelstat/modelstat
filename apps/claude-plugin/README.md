# modelstat for Claude Code

Charts of your AI usage, inside your session.

```bash
claude plugin marketplace add modelstat/modelstat
claude plugin install modelstat@modelstat
```

| Try | Get |
|---|---|
| `/stat` | 30-day dashboard: spend, daily trend, top models |
| `/stat session` | the session you're in — ingested on the spot, merged across compactions |
| `/stat models 7d` | model leaderboard |
| `/stat cache hit rate by day, 2 weeks` | anything `usage_explore` can answer |

Bundles the [`@modelstat/mcp`](https://www.npmjs.com/package/@modelstat/mcp) server, so the tools also answer plain questions ("how much did Opus cost me this month?") without `/stat`.

Requires a paired [modelstat](https://modelstat.ai/install) agent: `npx modelstat@latest`.
