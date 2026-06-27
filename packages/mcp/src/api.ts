/**
 * Thin wrapper around the modelstat MCP endpoints.
 *
 * This package ships two calls only:
 *   GET  /v1/mcp/tools — live tool catalog (replaces the static list)
 *   POST /v1/mcp/call  — invoke a tool; server returns an MCP
 *                        CallToolResult that we forward verbatim
 *
 * All per-tool logic, validation, and formatting live server-side. The
 * MCP package is a pass-through: auth + catalog + dispatcher.
 */
import type { State } from "./state.js";

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly body: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

export type McpToolDecl = {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
};

export type McpContent =
  | { type: "text"; text: string }
  | { type: "image"; data: string; mimeType: string }
  | { type: "resource"; resource: unknown };

export type McpCallResult = {
  content: McpContent[];
  isError?: boolean;
  structuredContent?: unknown;
};

export type McpPromptArg = {
  name: string;
  description?: string;
  required?: boolean;
};

export type McpPromptDecl = {
  name: string;
  description: string;
  arguments: McpPromptArg[];
};

export type McpPromptMessage = {
  role: string;
  content: { type: "text"; text: string };
};

export type McpGetPromptResult = {
  description?: string;
  messages: McpPromptMessage[];
};

function assertPaired(state: State): void {
  if (!state.bearer) {
    throw new Error(
      "modelstat is not paired on this machine. Run `npx modelstat@latest` to pair, or install the CLI first: https://modelstat.ai/install",
    );
  }
}

async function request<T>(
  state: State,
  method: "GET" | "POST",
  path: string,
  opts: { body?: unknown; timeoutMs?: number } = {},
): Promise<T> {
  assertPaired(state);
  const url = new URL(path.startsWith("/") ? path : `/${path}`, state.apiUrl);
  const signal =
    opts.timeoutMs != null ? AbortSignal.timeout(opts.timeoutMs) : undefined;
  const headers: Record<string, string> = {
    Authorization: `Bearer ${state.bearer}`,
    Accept: "application/json",
  };
  if (method === "POST") headers["content-type"] = "application/json";
  const res = await fetch(url, {
    method,
    headers,
    body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
    signal,
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new ApiError(
      `${res.status} ${res.statusText} for ${url.pathname}`,
      res.status,
      body,
    );
  }
  return (await res.json()) as T;
}

export const api = {
  /** Fetch the authoritative tool catalog. Short timeout — MCP clients
   * have their own startup timeouts and we'd rather fall back to the
   * static/cached list than block the whole launch. */
  listTools(
    state: State,
    opts: { timeoutMs?: number } = {},
  ): Promise<{ tools: McpToolDecl[] }> {
    return request(state, "GET", "/v1/mcp/tools", {
      timeoutMs: opts.timeoutMs ?? 1500,
    });
  },
  /** Forward a tool call. No client-side timeout — some tools may
   * legitimately be long-running; the MCP client decides when to give
   * up. */
  callTool(
    state: State,
    name: string,
    args: Record<string, unknown>,
  ): Promise<McpCallResult> {
    return request(state, "POST", "/v1/mcp/call", {
      body: { name, arguments: args },
    });
  },
  /** Fetch the authoritative prompt (slash-command) catalog. Short timeout for
   * the same reason as `listTools` — fall back to an empty list rather than
   * block launch. */
  listPrompts(
    state: State,
    opts: { timeoutMs?: number } = {},
  ): Promise<{ prompts: McpPromptDecl[] }> {
    return request(state, "GET", "/v1/mcp/prompts", {
      timeoutMs: opts.timeoutMs ?? 1500,
    });
  },
  /** Expand one prompt template server-side (`prompts/get`) and forward the
   * resulting messages verbatim. */
  getPrompt(
    state: State,
    name: string,
    args: Record<string, string>,
  ): Promise<McpGetPromptResult> {
    return request(state, "POST", "/v1/mcp/prompt", {
      body: { name, arguments: args },
    });
  },
};
