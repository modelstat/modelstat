import assert from "node:assert/strict";
import { test } from "node:test";
import type { McpToolDecl } from "./api.js";
import {
  WIDGET_MIME,
  WIDGET_TOOL,
  WIDGET_URI,
  widgetResourceEntry,
  withResultMeta,
  withWidgetMeta,
} from "./widget.js";

const tool = (name: string): McpToolDecl => ({
  name,
  description: `${name} desc`,
  inputSchema: { type: "object" },
});

test("withWidgetMeta tags only session_insights with the ui resource", () => {
  const out = withWidgetMeta([tool("overview"), tool(WIDGET_TOOL), tool("sessions")]);

  const overview = out.find((t) => t.name === "overview")!;
  assert.equal(overview._meta, undefined, "other tools are untouched");

  const insights = out.find((t) => t.name === WIDGET_TOOL)!;
  assert.deepEqual(insights._meta?.ui, { resourceUri: WIDGET_URI });
  // legacy flat alias set too, for older hosts
  assert.equal(insights._meta?.["ui/resourceUri"], WIDGET_URI);
  // original fields preserved
  assert.equal(insights.description, "session_insights desc");
});

test("withWidgetMeta does not mutate the input", () => {
  const input = [tool(WIDGET_TOOL)];
  const out = withWidgetMeta(input);
  assert.equal((input[0] as { _meta?: unknown })._meta, undefined);
  assert.notEqual(out[0], input[0]);
});

test("withResultMeta binds the result and preserves payload", () => {
  const result = {
    content: [{ type: "text" as const, text: "hi" }],
    structuredContent: { status: "ready", cost_usd: 0.5 },
  };
  const out = withResultMeta(result);
  assert.deepEqual(out._meta?.ui, { resourceUri: WIDGET_URI });
  assert.deepEqual(out.content, result.content);
  assert.deepEqual(out.structuredContent, result.structuredContent);
});

test("widgetResourceEntry describes the ui:// HTML resource", () => {
  const e = widgetResourceEntry();
  assert.equal(e.uri, WIDGET_URI);
  assert.equal(e.uri, "ui://modelstat/session-insights");
  assert.equal(e.mimeType, WIDGET_MIME);
  assert.equal(WIDGET_MIME, "text/html;profile=mcp-app");
});
