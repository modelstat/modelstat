#!/usr/bin/env node
/**
 * @modelstat/mcp — Model Context Protocol server for modelstat.
 *
 * This package is a thin stdio bridge: it advertises a tool catalog
 * and forwards every tool call to the modelstat backend verbatim. The
 * authoritative catalog lives at GET /v1/mcp/tools; each invocation
 * is POST /v1/mcp/call and the MCP-shaped {content: [...]} response
 * is passed through to the client without reshaping.
 *
 * The static TOOLS array below serves two jobs:
 *   1. Discoverability — npm search, GitHub code search, and MCP
 *      directories index the source. The static names/descriptions
 *      make the tool surface findable before a user has ever paired.
 *   2. Resilience — if the live /v1/mcp/tools endpoint is unreachable
 *      AND there's no on-disk cache from a previous success, we still
 *      hand clients a reasonable catalog.
 *
 * Remote fully replaces static when the live fetch succeeds. Adding
 * a new tool or tweaking a description is a backend deploy, not an
 * npm publish. A publish is only required for SDK bumps, auth
 * mechanism changes, or new MCP primitives (resources / prompts /
 * sampling) that this pass-through doesn't yet support.
 *
 * Usage (Claude Desktop ~/Library/Application Support/Claude/claude_desktop_config.json):
 *   {
 *     "mcpServers": {
 *       "modelstat": { "command": "npx", "args": ["-y", "@modelstat/mcp"] }
 *     }
 *   }
 *
 * Auth: reads the bearer token the agent CLI writes at pairing time.
 * The MCP server never prompts for credentials in-band.
 */
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { ApiError, api, type McpToolDecl } from "./api.js";
import { loadState, readToolsCache, writeToolsCache } from "./state.js";

const RANGES = ["today", "7d", "30d", "90d", "mtd", "ytd"] as const;
const DIMENSIONS = [
  "provider",
  "model",
  "tool",
  "day",
  "hour",
  "device",
  "identity",
  "session",
] as const;
const METRICS = [
  "cost",
  "list",
  "tokens",
  "events",
  "sessions",
  "tokens_input",
  "tokens_output",
  "tokens_cache_read",
  "tokens_cache_creation",
  "tokens_reasoning",
] as const;

const RANGE_PROPS = {
  range: {
    type: "string",
    enum: [...RANGES],
    description:
      "Named time window (ignored when from/to given). Omit range AND from/to for all-time.",
  },
  from: {
    type: "string",
    description: "RFC3339 inclusive lower bound (overrides `range`)",
  },
  to: {
    type: "string",
    description: "RFC3339 exclusive upper bound (overrides `range`)",
  },
};

/**
 * Static tool catalog — published in the npm tarball for discovery
 * (registry crawlers, GitHub search, LLM-assisted "find me an MCP
 * for X" queries) and used at runtime only when both the live fetch
 * and the on-disk cache are unavailable. See file header.
 *
 * Mirrors the authoritative server catalog — the server is the source
 * of truth; when a scraper or agent reads this file, this is what they
 * see.
 */
const TOOLS: McpToolDecl[] = [
  {
    name: "usage_overview",
    description:
      "Spend/usage headline for the account: effective cost, list-price cost, savings, total tokens, event count, distinct sessions. Start here for 'how much did I spend?'. Costs are exact decimal strings in USD.",
    inputSchema: { type: "object", properties: { ...RANGE_PROPS } },
  },
  {
    name: "usage_explore",
    description:
      "The charting workhorse: group-by (and optionally stack-by) any dimension, pick a metric, filter, and get back cells + whole-set totals. Time series: group_by=day or hour (cells come back chronologically — ideal for line/bar charts); stacked series: add stack_by=model|tool|provider. Leaderboards: group_by=model|tool|session etc. (sorted by value). Token-class metrics (tokens_input, tokens_output, tokens_cache_read, tokens_cache_creation, tokens_reasoning) split the raw token volume — e.g. cache-hit-rate = tokens_cache_read vs tokens. Filters (providers/models/tools/session_ids) are exact-match lists; session_ids scopes everything to those sessions (pass a whole compaction chain for one logical conversation). Cost values are exact decimal USD strings.",
    inputSchema: {
      type: "object",
      properties: {
        group_by: { type: "string", enum: [...DIMENSIONS], default: "day" },
        stack_by: {
          type: "string",
          enum: [...DIMENSIONS],
          description: "Optional second dimension; each cell carries `stack`.",
        },
        metric: { type: "string", enum: [...METRICS], default: "cost" },
        providers: {
          type: "array",
          items: { type: "string" },
          description: 'e.g. ["anthropic"]',
        },
        models: { type: "array", items: { type: "string" } },
        tools: {
          type: "array",
          items: { type: "string" },
          description: 'e.g. ["claude_code", "cursor"]',
        },
        session_ids: { type: "array", items: { type: "string" } },
        limit: {
          type: "integer",
          minimum: 1,
          maximum: 500,
          default: 50,
          description: "Top-N cap on returned groups.",
        },
        ...RANGE_PROPS,
      },
    },
  },
  {
    name: "list_sessions",
    description:
      "The account's sessions, most recent activity first (cursor-paginated). Each row: session_id, tool, total tokens, effective cost. Use session_detail or sessions_usage to drill in.",
    inputSchema: {
      type: "object",
      properties: {
        limit: { type: "integer", minimum: 1, maximum: 500 },
        cursor: { type: "string" },
      },
    },
  },
  {
    name: "session_detail",
    description:
      "One session with its full token breakdown and its segments (time-bounded slices with redacted abstracts + tags). The segment abstracts tell you WHAT the session was about.",
    inputSchema: {
      type: "object",
      required: ["session_id"],
      properties: { session_id: { type: "string" } },
    },
  },
  {
    name: "sessions_usage",
    description:
      "Aggregate usage over an EXPLICIT set of session ids: per-session rows (tool, time bounds, per-class tokens, cost) plus a combined roll-up. Built for 'current session' analysis in Claude Code: one logical conversation spans several session ids across compactions/resumes — pass every sessionId found in the transcript chain and read the combined block. Ids not (yet) ingested are listed in missing_session_ids.",
    inputSchema: {
      type: "object",
      required: ["session_ids"],
      properties: {
        session_ids: {
          type: "array",
          items: { type: "string" },
          minItems: 1,
          maxItems: 200,
        },
      },
    },
  },
  {
    name: "assign_session",
    description: "MUTATING: reassign a session's owner/identity.",
    inputSchema: {
      type: "object",
      required: ["session_id", "target"],
      properties: {
        session_id: { type: "string" },
        target: { type: "string", description: "identity/owner to assign" },
      },
    },
  },
];

const server = new Server({ name: "modelstat", version: "0.0.2" }, { capabilities: { tools: {} } });

// ─── ListTools: prefer live catalog, fall back gracefully ────────

server.setRequestHandler(ListToolsRequestSchema, async () => {
  const state = loadState();
  try {
    const live = await api.listTools(state, { timeoutMs: 1500 });
    writeToolsCache(state, live);
    log(`tools=remote count=${live.tools.length}`);
    return { tools: live.tools };
  } catch (err) {
    const reason = (err as Error).message;
    const cached = readToolsCache(state);
    if (cached) {
      log(`tools=cached count=${cached.tools.length} (remote=${reason})`);
      return { tools: cached.tools };
    }
    log(`tools=static count=${TOOLS.length} (remote=${reason})`);
    return { tools: TOOLS };
  }
});

// ─── CallTool: forward to backend, pass response through ─────────

server.setRequestHandler(CallToolRequestSchema, async (req) => {
  const state = loadState();
  const name = req.params.name;
  const args = (req.params.arguments ?? {}) as Record<string, unknown>;
  try {
    return await api.callTool(state, name, args);
  } catch (err) {
    if (err instanceof ApiError) {
      if (err.status === 401) {
        return errorResult(
          "modelstat API returned 401. Your bearer token may have expired — run `npx modelstat@latest` to re-pair.",
        );
      }
      if (err.status === 404) {
        return errorResult(
          `Tool \`${name}\` is no longer available — your MCP catalog may be out of date. Restart your MCP client to refresh.`,
        );
      }
      const detail = err.body ? `: ${err.body.slice(0, 400)}` : "";
      return errorResult(`modelstat API error (${err.status})${detail}`);
    }
    return errorResult((err as Error).message);
  }
});

function errorResult(text: string) {
  return { isError: true, content: [{ type: "text" as const, text }] };
}

function log(line: string): void {
  // stderr only — stdout is the MCP transport channel.
  process.stderr.write(`modelstat-mcp: ${line}\n`);
}

// ─── bootstrap ────────────────────────────────────────────────────

async function main(): Promise<void> {
  const transport = new StdioServerTransport();
  await server.connect(transport);
  process.stderr.write("modelstat-mcp: ready\n");
}

main().catch((e) => {
  process.stderr.write(`modelstat-mcp: fatal: ${(e as Error).message}\n`);
  process.exit(1);
});
