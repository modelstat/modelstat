---
description: Charts of your AI usage — spend, tokens, models, or the current session (via modelstat)
argument-hint: [session | models 7d | $ debugging <project> | any question about your usage]
allowed-tools: mcp__modelstat__overview, mcp__modelstat__explore, mcp__modelstat__sessions, mcp__modelstat__session_insights, mcp__modelstat__find_taxonomy, mcp__modelstat__find_projects, mcp__modelstat__find_people, mcp__modelstat__assign_session, Bash(modelstat *), Bash(ls:*), Bash(grep:*), Bash(pwd:*), Bash(sed:*), Bash(sort:*), Bash(uniq:*), Bash(head:*), Bash(wc:*)
---

Answer the user's usage-analytics question with data from the **modelstat MCP tools**, rendered as charts. Request: `$ARGUMENTS`

## The tool pattern: resolve, then filter

modelstat's analytics filter on **ids**, not names. So when a request names a project, person, or work-type, **resolve it first**, then pass the id(s) into `explore`/`sessions`:

- project name → `find_projects {q}` → a taxonomy node id (with spend).
- work-type / domain / any taxonomy name → `find_taxonomy {q, root_key?}` → node id(s).
- person / account → `find_people {q}` → identity id(s).

Then `explore`'s `taxonomy` filter takes **AND-of-OR** id groups: `[[a,b],[c]]` means tagged (a OR b) AND c; a flat array `[a,b]` is a single OR-group.

> "total $ debugging the acme project, last 30d" →
> `find_projects {q:"acme"}` → `PROJ`; `find_taxonomy {q:"debugging"}` → `DEBUG`;
> `explore {metric:"cost", range:"30d", group_by:"day", taxonomy:[[PROJ],[DEBUG]]}` → `totals.cost_usd` is the answer (+ the daily trend).

## Interpreting the request

- **Empty** → 30-day dashboard: `overview` (range `30d`) + `explore` (group_by `day`, metric `cost`) + `explore` (group_by `model`, metric `tokens`, limit 5). Render: headline numbers, daily trend chart, model leaderboard.
- **`session`** → the **current-session flow** below.
- **Anything else** (`models 7d`, "cache hit rate this month", "compare claude_code vs cursor by day", or a resolve-then-filter question as above) → map to `explore`:
  - time series → `group_by: "day"` (or `"hour"` for ≤2 days), optionally `stack_by: "model" | "tool" | "provider"`
  - leaderboards → `group_by: "model" | "tool" | "provider" | "session" | "identity"`
  - metrics: `cost`, `list`, `tokens`, `events`, `sessions`, or token classes `tokens_input`, `tokens_output`, `tokens_cache_read`, `tokens_cache_creation`, `tokens_reasoning` (cache hit rate = `tokens_cache_read` ÷ `tokens`)
  - ranges: presets `today|7d|30d|90d|mtd|ytd`, or RFC3339 `from`/`to` for anything else ("last Tuesday", "April") — compute the dates yourself.
  - filters: `providers`, `models`, `tools` (slugs like `claude_code`, `cursor`), `identities`, `taxonomy` (resolved ids), `session_ids`.

Every `explore` response includes `totals` — always show the total alongside the breakdown. Costs are exact decimal USD strings.

## Current-session flow (`/stat session`)

This uses **`session_insights`**, which eagerly force-scans the current session (via the local daemon) and returns its tokens, effective $ assigned, and the taxonomy nodes detected — with a `status` of `ready` | `analyzing` | `not_ingested`.

One logical conversation spans multiple session ids across compactions/resumes; the transcript's per-line `sessionId` values are the chain.

1. Resolve the transcript and chain ids (one Bash call):
   ```bash
   DIR="$HOME/.claude/projects/$(pwd | sed 's/[^a-zA-Z0-9]/-/g')"
   FILE=$(ls -t "$DIR"/*.jsonl | head -1)
   grep -ho '"sessionId":"[^"]*"' "$FILE" | sed 's/.*:"//;s/"//' | sort -u
   ```
2. Call `session_insights` with **`eager: true`** and ALL chain ids as `session_ids`. The eager call force-scans the session locally first, so the first response already reflects it.
3. **Re-poll while `status == "analyzing"`**: call `session_insights` again with the same ids and **`eager: false`** every ~2.5s, up to ~20s (≈8 tries), until `status` becomes `ready` (or you hit the cap — then render what you have and note "taxonomy still processing"). Taxonomy detection runs after tokens/cost land, so `analyzing` means "$ and tokens are ready, work-types are still being tagged."
   - `status: "not_ingested"` (and `missing_session_ids`) → the daemon hasn't shipped this session yet. If it persists, the daemon may not be paired — tell the user to run `modelstat` (or install it: `curl -fsSL https://modelstat.ai/install.sh | sh`), or `modelstat sync --session <id>` to force it, and stop.
4. **Render** the result:
   - If the MCP host shows a `session_insights` widget (Claude Desktop / MCP-UI hosts render it from the tool result automatically), let it stand and add a one-line summary.
   - Otherwise (Claude Code) render the returned `structuredContent` / text as markdown: session header (ids, span, segment count), a token-split chart (input / output / cache read / cache create), the effective $ (and saved vs list if present), and the taxonomy chips detected.
5. Local extra the server doesn't have — top tool calls from the transcript:
   ```bash
   grep -o '"type":"tool_use"[^}]*"name":"[^"]*"' "$FILE" | grep -o '"name":"[^"]*"' | sed 's/.*:"//;s/"//' | sort | uniq -c | sort -rn | head -10
   ```
6. Optional: `explore` with `session_ids: [chain]`, `group_by: "hour"` for the session's burn-rate curve.

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

If a tool errors with 401/not-paired, the fix is `modelstat` (connects the device — the MCP can also connect itself via the browser on first use; install with `curl -fsSL https://modelstat.ai/install.sh | sh`). If results look empty, note ingest lag — the daemon ingests automatically; force a specific session with `modelstat sync --session <id>`.
