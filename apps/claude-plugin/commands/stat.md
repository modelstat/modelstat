---
description: Charts of your AI usage — spend, tokens, models, or the current session (via modelstat)
argument-hint: [session | models 7d | any question about your usage]
allowed-tools: mcp__modelstat__usage_overview, mcp__modelstat__usage_explore, mcp__modelstat__list_sessions, mcp__modelstat__session_detail, mcp__modelstat__sessions_usage, Bash(npx -y modelstat@latest *), Bash(ls:*), Bash(grep:*), Bash(pwd:*), Bash(sed:*), Bash(sort:*), Bash(uniq:*), Bash(head:*), Bash(wc:*)
---

Answer the user's usage-analytics question with data from the **modelstat MCP tools**, rendered as charts. Request: `$ARGUMENTS`

## Interpreting the request

- **Empty** → 30-day dashboard: `usage_overview` (range `30d`) + `usage_explore` (group_by `day`, metric `cost`) + `usage_explore` (group_by `model`, metric `tokens`, limit 5). Render: headline numbers, daily trend chart, model leaderboard.
- **`session`** → the **current-session flow** below.
- **Anything else** (e.g. `models 7d`, "cache hit rate this month", "compare claude_code vs cursor by day") → map to `usage_explore`:
  - time series → `group_by: "day"` (or `"hour"` for ≤2 days), optionally `stack_by: "model" | "tool" | "provider"`
  - leaderboards → `group_by: "model" | "tool" | "provider" | "session"`
  - metrics: `cost`, `list`, `tokens`, `events`, `sessions`, or token classes `tokens_input`, `tokens_output`, `tokens_cache_read`, `tokens_cache_creation`, `tokens_reasoning` (cache hit rate = `tokens_cache_read` ÷ `tokens`)
  - ranges: presets `today|7d|30d|90d|mtd|ytd`, or RFC3339 `from`/`to` for anything else ("last Tuesday", "April") — compute the dates yourself.
  - filters: `providers`, `models`, `tools` (slugs like `claude_code`, `cursor`), `session_ids`.

Every `usage_explore` response includes `totals` — always show the total alongside the breakdown. Costs are exact decimal USD strings.

## Current-session flow (`/stat session`)

One logical conversation spans multiple session ids across compactions/resumes; the transcript's per-line `sessionId` values are the chain.

1. Resolve the transcript and chain ids (one Bash call):
   ```bash
   DIR="$HOME/.claude/projects/$(pwd | sed 's/[^a-zA-Z0-9]/-/g')"
   FILE=$(ls -t "$DIR"/*.jsonl | head -1)
   grep -ho '"sessionId":"[^"]*"' "$FILE" | sed 's/.*:"//;s/"//' | sort -u
   ```
2. Ingest the latest activity: `npx -y modelstat@latest scan` (takes a few seconds; if it reports the device isn't paired, tell the user to run `npx modelstat@latest` first and stop).
3. Call `sessions_usage` with ALL chain ids → combined tokens (per class), cost, duration, segments. Ids in `missing_session_ids` just haven't been ingested/processed yet — say so.
4. Local extras the server doesn't have yet — tool-call counts from the transcript:
   ```bash
   grep -o '"type":"tool_use"[^}]*"name":"[^"]*"' "$FILE" | grep -o '"name":"[^"]*"' | sed 's/.*:"//;s/"//' | sort | uniq -c | sort -rn | head -10
   ```
5. Optionally `usage_explore` with `session_ids: [chain]`, `group_by: "hour"` for the session's burn-rate curve, or `session_detail` for segment abstracts ("what was this session about").

Render: session header (ids, span, segments), token split chart (input / output / cache read / cache create), cost, top tools used, burn-by-hour sparkline.

## Rendering charts

If a rich visualization tool is available in this session (e.g. `mcp__visualize__show_widget`), use it for the main chart. Otherwise draw crisp terminal charts:

```
Daily spend (30d)                    total $42.18
Jun 01  ███████████████████▌  $3.94
Jun 02  ██████▏               $1.23
Jun 03  ████████████▊         $2.57
```

- Horizontal bars `█▉▊▋▌▍▎▏`, scaled to the max value, ~24 chars wide; right-align numbers.
- Trends/sparklines: `▁▂▃▄▅▆▇█`.
- Stacked series: one bar per group with per-stack colored segments isn't possible — use a compact table or repeat bars per stack key, whichever reads better for the data size.
- Tokens ≥10k → `12.3k` / `1.2M`. Costs `$X.XX`.
- Keep it tight: one chart + 2–4 insight bullets (biggest mover, cache hit rate, outliers). No filler prose.

If a tool errors with 401/not-paired, the fix is `npx modelstat@latest` (pairs the device). If results look empty, suggest `npx -y modelstat@latest scan` and note ingest lag.
