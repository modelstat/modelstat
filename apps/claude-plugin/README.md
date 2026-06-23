# modelstat for Claude Code

Charts of your AI usage, inside your session — dollar-precise spend broken down by the real work it went to.

```bash
claude plugin marketplace add modelstat/modelstat
claude plugin install modelstat@modelstat
```

<!-- dashboard screenshot: drop assets/activities-screenshot.png here when available -->

| Try | Get |
|---|---|
| `/stat` | 30-day dashboard: spend, daily trend, top models |
| `/stat session` | the session you're in — eagerly scanned + merged across compactions: tokens, $ assigned, work-types detected |
| `/stat models 7d` | model leaderboard |
| `/stat $ debugging acme` | resolve names → ids, then filter — anything `explore` can answer |
| `/stat cache hit rate by day, 2 weeks` | per-class token math over any range |

**Where the numbers come from** — the same on-device pipeline that powers the dashboard:

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

Bundles the [`@modelstat/mcp`](https://www.npmjs.com/package/@modelstat/mcp) server, so the tools also answer plain questions ("how much did Opus cost me this month?") without `/stat`.

Works with a paired [modelstat](https://modelstat.ai/install) daemon (`npx modelstat@latest`) — or, with no daemon, the MCP connects itself via your browser on first use.
