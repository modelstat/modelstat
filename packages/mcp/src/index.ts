#!/usr/bin/env node
/**
 * @modelstat/mcp — Model Context Protocol server for modelstat.
 *
 * A thin stdio bridge: it advertises a tool catalog and forwards every tool
 * call to the modelstat backend verbatim. The authoritative catalog lives at
 * GET /v1/mcp/tools; each invocation is POST /v1/mcp/call and the MCP-shaped
 * {content, isError, structuredContent} response is passed through to the
 * client without reshaping (so a new server tool — or richer result, including
 * structuredContent for a widget — appears automatically, no npm publish).
 *
 * The static TOOLS array below serves two jobs:
 *   1. Discoverability — npm / GitHub / MCP-directory crawlers index the
 *      source, so the surface is findable before a user has paired.
 *   2. Resilience — when the live catalog AND the on-disk cache are both
 *      unavailable, clients still get a reasonable list.
 * Remote fully replaces static once the live fetch succeeds.
 *
 * Auth (state.ts): the local daemon's bearer is the fast path; with no daemon
 * we run a one-time browser claim (auth.ts) and persist our own bearer. So the
 * MCP works WITHOUT a daemon installed.
 *
 * Usage (Claude Desktop claude_desktop_config.json / Claude Code .mcp.json):
 *   { "mcpServers": { "modelstat": { "command": "npx", "args": ["-y", "@modelstat/mcp"] } } }
 *
 * TODO (deferred — see PR notes): an MCP-UI HTML widget for session_insights
 * (ui://modelstat/session-insights). The installed @modelcontextprotocol/sdk
 * (1.x) has no MCP-UI primitive, and MCP-UI lives in a separate, still-evolving
 * package (@mcp-ui/server / @modelcontextprotocol/ext-apps); associating a
 * widget with a tool result also needs the SERVER to stamp the result `_meta`.
 * Rather than ship guessed/broken widget code, the tool returns the server's
 * well-formatted TEXT + structuredContent (works everywhere incl Claude Code).
 */
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { ApiError, api, type McpToolDecl } from "./api.js";
import { browserClaim } from "./auth.js";
import { kickDaemonScan } from "./control.js";
import { loadState, type State, readToolsCache, writeToolsCache } from "./state.js";
import { runWire } from "./wire.js";

const RANGES = ["today", "7d", "30d", "90d", "mtd", "ytd"] as const;
// Dimensions the explore tool can group/stack by — mirrors the server catalog.
const DIMENSIONS = [
  "provider",
  "model",
  "tool",
  "day",
  "hour",
  "device",
  "identity",
  "session",
  "taxonomy",
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
  from: { type: "string", description: "RFC3339 inclusive lower bound (overrides `range`)" },
  to: { type: "string", description: "RFC3339 exclusive upper bound (overrides `range`)" },
};

/**
 * Static tool catalog — published for discovery + used at runtime only when
 * both the live fetch and the on-disk cache are unavailable. Mirrors the
 * authoritative server catalog (explore, overview, sessions, session_insights,
 * find_taxonomy, find_projects, find_people, assign_session). The agent
 * pattern: resolve names → ids with the find_* tools, then pass ids as filters
 * to explore/sessions.
 */
const TOOLS: McpToolDecl[] = [
  {
    name: "overview",
    description:
      "Headline spend/usage for the account: effective cost, list-price cost, savings, total tokens, event count, distinct sessions, ROI (repos/PRs), and taxonomy roots. Start here for 'how much did I spend?'. Costs are exact decimal USD strings.",
    inputSchema: { type: "object", properties: { ...RANGE_PROPS } },
  },
  {
    name: "explore",
    description:
      "The analytics workhorse (event/segment grain): group-by (and optionally stack-by) any dimension, pick a metric, filter, get back cells + whole-set totals. Time series: group_by=day|hour. Leaderboards: group_by=model|tool|session|identity. The `taxonomy` filter takes AND-of-OR groups [[idA,idB],[idC]] (AND across groups, OR within; a flat array is one OR-group) — resolve ids first with find_taxonomy / find_projects. This is how you answer cross-cutting questions like 'total $ debugging the acme project': find both ids, then explore with taxonomy:[[proj],[debug]]. Costs are exact decimal USD strings.",
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
        taxonomy: {
          type: "array",
          description:
            "AND-of-OR taxonomy node-id groups, e.g. [[projId],[debugId]] = tagged BOTH. A flat array [a,b] is one OR-group. Auto-expanded to subtrees server-side.",
          items: {
            oneOf: [
              { type: "string" },
              { type: "array", items: { type: "string" } },
            ],
          },
        },
        providers: { type: "array", items: { type: "string" }, description: 'e.g. ["anthropic"]' },
        models: { type: "array", items: { type: "string" } },
        tools: { type: "array", items: { type: "string" }, description: 'e.g. ["claude_code"]' },
        identities: { type: "array", items: { type: "string" }, description: "identity ids" },
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
    name: "sessions",
    description:
      "Search/list sessions, most recent first (cursor-paginated). Filter by taxonomy AND-of-OR groups, project, device, identity, free-text `q`, and range. Each row: session_id, tool, total tokens, effective cost. Resolve names to ids with find_* first.",
    inputSchema: {
      type: "object",
      properties: {
        q: { type: "string", description: "free-text match over session abstracts/metadata" },
        taxonomy: {
          type: "array",
          description: "AND-of-OR taxonomy node-id groups (see explore.taxonomy).",
          items: {
            oneOf: [{ type: "string" }, { type: "array", items: { type: "string" } }],
          },
        },
        identity_id: { type: "string" },
        device_id: { type: "string" },
        limit: { type: "integer", minimum: 1, maximum: 500 },
        cursor: { type: "string" },
        ...RANGE_PROPS,
      },
    },
  },
  {
    name: "session_insights",
    description:
      "Live per-session insights for the CURRENT session: total tokens, effective $ assigned, and the taxonomy nodes detected, plus a status (ready | analyzing | not_ingested). Pass every session id in the transcript chain (compactions/resumes are one logical conversation). Set eager:true to force-scan the session locally first (via a running daemon) and prioritise server enrichment — then re-call with eager:false every ~2.5s while status==\"analyzing\" (up to ~20s). Returns a formatted text summary + structuredContent for rendering.",
    inputSchema: {
      type: "object",
      required: ["session_ids"],
      properties: {
        session_ids: {
          type: "array",
          items: { type: "string" },
          minItems: 1,
          maxItems: 200,
          description: "The session-id chain for one logical conversation.",
        },
        eager: {
          type: "boolean",
          default: false,
          description:
            "Force-scan the session on the local daemon first + prioritise server enrichment. Use once, then poll with eager:false.",
        },
      },
    },
  },
  {
    name: "find_taxonomy",
    description:
      "Resolve taxonomy node names → ids. Search by name, optionally scoped to a root_key (e.g. work_type). Returns [{id,name,path,root_key,…}]. Feed the ids into explore/sessions `taxonomy` filters.",
    inputSchema: {
      type: "object",
      properties: {
        q: { type: "string", description: "name search, e.g. \"debugging\"" },
        root_key: { type: "string", description: "restrict to one taxonomy root" },
        limit: { type: "integer", minimum: 1, maximum: 100 },
      },
    },
  },
  {
    name: "find_projects",
    description:
      "List/search projects (the `workstreams` taxonomy root) → node ids, with spend. Returns [{id,name,slug,cost_usd,sessions}] where `id` is a taxonomy node id usable directly as a filter in explore/sessions.",
    inputSchema: {
      type: "object",
      properties: {
        q: { type: "string", description: "project name search" },
        limit: { type: "integer", minimum: 1, maximum: 100 },
        ...RANGE_PROPS,
      },
    },
  },
  {
    name: "find_people",
    description:
      "Search provider-account identities → ids for the `identities` filter. Returns matching identities you can then pass to explore/sessions.",
    inputSchema: {
      type: "object",
      properties: {
        q: { type: "string", description: "name/email/handle search" },
        limit: { type: "integer", minimum: 1, maximum: 100 },
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

const server = new Server({ name: "modelstat", version: "0.0.3" }, { capabilities: { tools: {} } });

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

// ─── CallTool: (eager pre-scan) → forward → pass response through ─

server.setRequestHandler(CallToolRequestSchema, async (req) => {
  let state = loadState();
  const name = req.params.name;
  const args = (req.params.arguments ?? {}) as Record<string, unknown>;

  // No local bearer anywhere → run the one-time browser claim, then continue.
  if (state.source === "none") {
    state = await browserClaim(state);
    if (state.source === "none") {
      return errorResult(
        "modelstat isn't connected yet. Sign in via the browser tab we opened (or run `npx modelstat@latest`), then retry.",
      );
    }
  }

  // session_insights + eager: force-scan the current session on the local
  // daemon FIRST (so the server sees fresh data), then forward. No daemon →
  // proceed anyway; the server returns not_ingested/analyzing.
  if (name === "session_insights" && args.eager === true) {
    await maybeEagerScan(args);
  }

  try {
    return await api.callTool(state, name, args);
  } catch (err) {
    if (err instanceof ApiError) {
      if (err.status === 401) {
        return errorResult(
          "modelstat API returned 401. Your token may have expired — run `npx modelstat@latest` to re-pair, or retry to re-claim.",
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

/** Kick the local daemon to force-scan the session ids in a session_insights
 * call. Best-effort: a missing daemon or a slow scan never blocks the forward. */
async function maybeEagerScan(args: Record<string, unknown>): Promise<void> {
  const ids = Array.isArray(args.session_ids)
    ? args.session_ids.filter((s): s is string => typeof s === "string" && s.length > 0)
    : [];
  if (ids.length === 0) return;
  const r = await kickDaemonScan(ids, { wait: true });
  if (r.kind === "no_daemon") {
    log("eager: no local daemon on the control port — proceeding (server will report status)");
  } else if (r.kind === "error") {
    log(`eager: daemon scan skipped (${r.message}) — proceeding`);
  } else {
    log("eager: daemon force-scanned the session");
  }
}

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
  // One-line auth provenance to stderr so users can sanity-check the source.
  const s: State = loadState();
  process.stderr.write(`modelstat-mcp: ready (auth=${s.source})\n`);
}

if (process.argv[2] === "wire") {
  // `npx -y @modelstat/mcp wire` — configure the modelstat MCP into the local
  // AI tools we can find, print a report, and exit (no server).
  runWire()
    .then((code) => process.exit(code))
    .catch((e) => {
      process.stderr.write(`modelstat-mcp: wire failed: ${(e as Error).message}\n`);
      process.exit(1);
    });
} else {
  main().catch((e) => {
    process.stderr.write(`modelstat-mcp: fatal: ${(e as Error).message}\n`);
    process.exit(1);
  });
}
