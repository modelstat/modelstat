/**
 * MCP Apps (MCP-UI) wiring for the `session_insights` tool.
 *
 * The bridge advertises one UI resource — `ui://modelstat/session-insights` —
 * and tags the `session_insights` tool with `_meta.ui.resourceUri`. An
 * MCP-Apps host (Claude Desktop / web) then fetches that resource and renders
 * the View (src/view/main.ts, bundled to dist/session-insights.html) in a
 * sandboxed iframe, feeding it the tool result over postMessage.
 *
 * Everything here is server-side glue; the visual lives in src/view. These
 * helpers are pure (apart from `loadWidgetHtml`'s one file read) so they can be
 * unit-tested without standing up the stdio server.
 *
 * Spec: https://github.com/modelcontextprotocol/ext-apps (2026-01-26).
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import type { McpCallResult, McpToolDecl } from "./api.js";

/** The tool whose result renders as the widget. */
export const WIDGET_TOOL = "session_insights";
/** UI resource URI the tool points at. */
export const WIDGET_URI = "ui://modelstat/session-insights";
/** MCP Apps HTML resource MIME type (RESOURCE_MIME_TYPE in @modelcontextprotocol/ext-apps). */
export const WIDGET_MIME = "text/html;profile=mcp-app";

/**
 * UI metadata stamped onto the tool. `_meta.ui.resourceUri` is the current key;
 * the flat `ui/resourceUri` is the legacy alias — we set both so older and
 * newer hosts both bind the widget (mirrors ext-apps `registerAppTool`).
 */
const TOOL_UI_META = {
  ui: { resourceUri: WIDGET_URI },
  "ui/resourceUri": WIDGET_URI,
} as const;

type ToolWithMeta = McpToolDecl & { _meta?: Record<string, unknown> };
type ResultWithMeta = McpCallResult & { _meta?: Record<string, unknown> };

/** Tag the `session_insights` tool in a catalog with its UI resource. */
export function withWidgetMeta(tools: McpToolDecl[]): ToolWithMeta[] {
  return tools.map((t) =>
    t.name === WIDGET_TOOL
      ? { ...t, _meta: { ...(t as ToolWithMeta)._meta, ...TOOL_UI_META } }
      : t,
  );
}

/** Bind a `session_insights` call result to the widget (belt-and-suspenders
 * alongside the tool-level meta — some hosts read it off the result). */
export function withResultMeta(result: McpCallResult): ResultWithMeta {
  return { ...result, _meta: { ...(result as ResultWithMeta)._meta, ...TOOL_UI_META } };
}

/** The `resources/list` entry for the widget. */
export function widgetResourceEntry() {
  return {
    uri: WIDGET_URI,
    name: "Session insights",
    description:
      "Live card for the current conversation: tokens, $ assigned, and detected work types.",
    mimeType: WIDGET_MIME,
    _meta: { ui: { prefersBorder: false } },
  };
}

let htmlCache: string | null = null;

/** Read the bundled View HTML (dist/session-insights.html, a sibling of the
 * built bridge). Cached after first read. */
export function loadWidgetHtml(): string {
  if (htmlCache != null) return htmlCache;
  const path = fileURLToPath(new URL("./session-insights.html", import.meta.url));
  htmlCache = readFileSync(path, "utf8");
  return htmlCache;
}

/** The `resources/read` result for the widget URI. */
export function readWidgetResource() {
  return {
    contents: [
      {
        uri: WIDGET_URI,
        mimeType: WIDGET_MIME,
        text: loadWidgetHtml(),
        _meta: { ui: { prefersBorder: false } },
      },
    ],
  };
}
