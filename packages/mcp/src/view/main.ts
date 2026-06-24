/**
 * modelstat session-insights View — the MCP App (MCP-UI) widget entry.
 *
 * Runs inside the host's sandboxed iframe (Claude Desktop / web, and any other
 * MCP-Apps host). It connects to the host over postMessage via the ext-apps
 * `App`, receives the `session_insights` tool result (the same payload the tool
 * returns as `structuredContent`), and renders the card (see render.ts).
 *
 * While the server is still classifying (`status: "analyzing"`) it re-polls the
 * tool through the host (`callServerTool`) until it settles, so the card fills
 * in live without the user re-invoking anything.
 *
 * Bundled to a single self-contained HTML document by scripts/build-view.mjs
 * (→ dist/session-insights.html) and served as ui://modelstat/session-insights
 * (see ../widget.ts).
 */
import {
  App,
  applyDocumentTheme,
  applyHostFonts,
  applyHostStyleVariables,
  type McpUiHostContext,
} from "@modelcontextprotocol/ext-apps";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import { type Insights, render, renderError } from "./render.js";

const root = document.getElementById("root") as HTMLElement;
const app = new App({ name: "modelstat-session-insights", version: "1.0.0" });

let sessionIds: string[] = [];
let polling = false;

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

function ingest(result: CallToolResult | undefined): void {
  const sc = result?.structuredContent as Insights | undefined;
  if (!sc || typeof sc.status !== "string") {
    renderError(root, "No insights in the tool result.");
    return;
  }
  if (Array.isArray(sc.session_ids) && sc.session_ids.length) {
    sessionIds = sc.session_ids;
  }
  render(root, sc);
  void maybePoll(sc.status);
}

/** Re-poll the tool through the host while the server is still classifying. */
async function maybePoll(status: string): Promise<void> {
  if (status !== "analyzing" || polling) return;
  if (!app.getHostCapabilities()?.serverTools) return; // host can't proxy tools
  if (!sessionIds.length) return;
  polling = true;
  const deadline = Date.now() + 25_000;
  try {
    while (Date.now() < deadline) {
      await sleep(2500);
      const res = await app.callServerTool({
        name: "session_insights",
        arguments: { session_ids: sessionIds, eager: false },
      });
      const next = res.structuredContent as Insights | undefined;
      if (!next) break;
      render(root, next);
      if (next.status !== "analyzing") break;
    }
  } catch (err) {
    console.error("session-insights: poll failed", err);
  } finally {
    polling = false;
  }
}

function applyHostContext(ctx: McpUiHostContext): void {
  if (ctx.theme) applyDocumentTheme(ctx.theme);
  if (ctx.styles?.variables) applyHostStyleVariables(ctx.styles.variables);
  if (ctx.styles?.css?.fonts) applyHostFonts(ctx.styles.css.fonts);
}

app.addEventListener("toolresult", (params) => ingest(params));
app.addEventListener("hostcontextchanged", (ctx) => applyHostContext(ctx));
app.onerror = (err) => console.error("session-insights:", err);

// Placeholder until the host delivers the first tool result.
render(root, { status: "analyzing", segments_pending: 0 });

app
  .connect()
  .then(() => {
    const ctx = app.getHostContext();
    if (ctx) applyHostContext(ctx);
  })
  .catch((err) => console.error("session-insights: connect failed", err));
