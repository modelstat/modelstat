//! The embedded stdio MCP bridge (§12) — a port of `packages/mcp/src/index.ts`.
//! A thin JSON-RPC 2.0 server over stdin/stdout: it advertises the tool catalog
//! (live → cache → static) and forwards every `tools/call` to `POST /v1/mcp/call`
//! verbatim, so a new server tool (or richer result) appears with no re-release.
//!
//! stdout carries the protocol EXCLUSIVELY; all logs go to stderr prefixed
//! `modelstat-mcp: `. The RPC dispatch is pure over an injected [`McpBackend`]
//! (tested with a fake); the runtime wires the real HTTP backend + auth.

use serde_json::{json, Value};

use crate::catalog::{static_tools, with_widget_meta, WIDGET_TOOL, WIDGET_URI};

/// The MCP protocol version this bridge speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// A `POST /v1/mcp/call` (or prompt) API error — status + optional body excerpt.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub body: Option<String>,
}

/// The backend seam: the HTTP forward + eager kick, injected so the dispatch is
/// testable without a network.
pub trait McpBackend {
    /// `GET /v1/mcp/tools` (with the on-disk cache) → the tool list, or `None`
    /// when both live + cache are unavailable (caller falls back to static).
    async fn list_tools(&self) -> Option<Vec<Value>>;
    /// `POST /v1/mcp/call {name, arguments}` → the MCP result, passed through.
    async fn call_tool(&self, name: &str, args: Value) -> Result<Value, ApiError>;
    /// `GET /v1/mcp/prompts` → the prompt list ( `[]` on failure — never a
    /// silent-empty that hides an error from the caller's log).
    async fn list_prompts(&self) -> Vec<Value>;
    /// `POST /v1/mcp/prompt {name, arguments}` → the expanded prompt.
    async fn get_prompt(&self, name: &str, args: Value) -> Result<Value, ApiError>;
    /// Best-effort loopback `POST /v1/control/scan {session_ids, wait:true}` — the
    /// `session_insights` eager pre-scan. Never blocks the forward.
    async fn eager_scan(&self, session_ids: Vec<String>);
    /// Whether any bearer is resolvable (env / identity / mcp-auth). `false` →
    /// the tool call returns a "not connected" result instead of forwarding.
    fn has_auth(&self) -> bool;
}

/// An MCP `{isError:true, content:[{type:text,...}]}` result.
pub fn error_result(text: &str) -> Value {
    json!({ "isError": true, "content": [ { "type": "text", "text": text } ] })
}

/// The widget resource list entry (§12).
fn widget_resource_entry() -> Value {
    json!({
        "uri": WIDGET_URI,
        "name": "modelstat session insights",
        "mimeType": "text/html;profile=mcp-app"
    })
}

/// The embedded, self-contained widget HTML (§12 — served over stdio; MCP-Apps
/// hosts render it from the tool's `structuredContent`). Minimal + dependency-free.
const WIDGET_HTML: &str = include_str!("widget.html");

/// Dispatch one JSON-RPC request against the backend, returning the response
/// value — or `None` for a notification (no `id`, no reply). Pure over the
/// backend. Port of the SDK request handlers in `index.ts`.
pub async fn handle_request<B: McpBackend>(req: &Value, backend: &B, version: &str) -> Option<Value> {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) get no reply.
    if id.is_none() {
        return None;
    }
    let id = id.unwrap();

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": { "name": "modelstat", "version": version },
            "capabilities": { "tools": {}, "resources": {}, "prompts": {} }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => {
            let tools = backend.list_tools().await.unwrap_or_else(static_tools);
            Ok(json!({ "tools": with_widget_meta(tools) }))
        }
        "resources/list" => Ok(json!({ "resources": [ widget_resource_entry() ] })),
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
            if uri == WIDGET_URI {
                Ok(json!({ "contents": [ {
                    "uri": WIDGET_URI,
                    "mimeType": "text/html;profile=mcp-app",
                    "text": WIDGET_HTML
                } ] }))
            } else {
                Err((-32602, format!("Unknown resource: {uri}")))
            }
        }
        "prompts/list" => Ok(json!({ "prompts": backend.list_prompts().await })),
        "prompts/get" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match backend.get_prompt(name, args).await {
                Ok(v) => Ok(v),
                Err(e) => Err((-32603, format!("prompt {name} failed ({})", e.status))),
            }
        }
        "tools/call" => Ok(handle_tool_call(&params, backend).await),
        other => Err((-32601, format!("Method not found: {other}"))),
    };

    Some(match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

/// The `tools/call` body: auth-gate → (eager pre-scan) → forward → pass through,
/// with the §12 error mapping. Always returns a tool RESULT (a forwarding failure
/// becomes an `isError` result, not a JSON-RPC error).
async fn handle_tool_call<B: McpBackend>(params: &Value, backend: &B) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    if !backend.has_auth() {
        return error_result(
            "modelstat isn't connected yet. Run `modelstat` to pair this device, then retry.",
        );
    }

    // session_insights + eager: force-scan the current session on the local daemon
    // FIRST (so the server sees fresh data), then forward.
    if name == "session_insights" && args.get("eager").and_then(Value::as_bool) == Some(true) {
        let ids: Vec<String> = args
            .get("session_ids")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if !ids.is_empty() {
            backend.eager_scan(ids).await;
        }
    }

    match backend.call_tool(name, args).await {
        Ok(result) => {
            // The widget tool gets its `_meta.ui.resourceUri` stamped on the result.
            if name == WIDGET_TOOL {
                with_result_meta(result)
            } else {
                result
            }
        }
        Err(e) => match e.status {
            401 => error_result(
                "modelstat API returned 401. Your token may have expired — run `modelstat` to re-pair, then retry.",
            ),
            404 => error_result(&format!(
                "Tool `{name}` is no longer available — your MCP catalog may be out of date. Restart your MCP client to refresh."
            )),
            status => {
                let detail = e
                    .body
                    .as_deref()
                    .map(|b| format!(": {}", &b.chars().take(400).collect::<String>()))
                    .unwrap_or_default();
                error_result(&format!("modelstat API error ({status}){detail}"))
            }
        },
    }
}

/// Stamp the widget `_meta.ui.resourceUri` onto a tool result (§12).
fn with_result_meta(mut result: Value) -> Value {
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "_meta".to_string(),
            json!({ "ui": { "resourceUri": WIDGET_URI }, "ui.resourceUri": WIDGET_URI }),
        );
    }
    result
}

/// Log one line to stderr (stdout is the protocol channel).
pub fn log(line: &str) {
    eprintln!("modelstat-mcp: {line}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake backend recording calls + returning scripted results.
    #[derive(Default)]
    struct Fake {
        tools: Option<Vec<Value>>,
        call_result: Option<Result<Value, ApiError>>,
        eager_ids: RefCell<Vec<String>>,
        authed: bool,
    }
    impl McpBackend for Fake {
        async fn list_tools(&self) -> Option<Vec<Value>> {
            self.tools.clone()
        }
        async fn call_tool(&self, _name: &str, _args: Value) -> Result<Value, ApiError> {
            self.call_result
                .clone()
                .unwrap_or_else(|| Ok(json!({ "content": [] })))
        }
        async fn list_prompts(&self) -> Vec<Value> {
            vec![]
        }
        async fn get_prompt(&self, _name: &str, _args: Value) -> Result<Value, ApiError> {
            Ok(json!({ "messages": [] }))
        }
        async fn eager_scan(&self, ids: Vec<String>) {
            *self.eager_ids.borrow_mut() = ids;
        }
        fn has_auth(&self) -> bool {
            self.authed
        }
    }

    fn req(id: i64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[tokio::test]
    async fn initialize_reports_the_server_identity_and_version() {
        let b = Fake::default();
        let resp = handle_request(&req(1, "initialize", json!({})), &b, "9.9.9")
            .await
            .unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "modelstat");
        assert_eq!(resp["result"]["serverInfo"]["version"], "9.9.9");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn tools_list_falls_back_to_static_and_tags_the_widget() {
        let b = Fake::default(); // list_tools → None → static
        let resp = handle_request(&req(2, "tools/list", json!({})), &b, "9.9.9")
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 8);
        let si = tools.iter().find(|t| t["name"] == "session_insights").unwrap();
        assert_eq!(si["_meta"]["ui"]["resourceUri"], WIDGET_URI);
    }

    #[tokio::test]
    async fn tools_list_prefers_the_live_catalog() {
        let b = Fake {
            tools: Some(vec![json!({ "name": "brand_new", "inputSchema": {} })]),
            authed: true,
            ..Default::default()
        };
        let resp = handle_request(&req(3, "tools/list", json!({})), &b, "9.9.9")
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "brand_new");
    }

    #[tokio::test]
    async fn tool_call_without_auth_returns_a_connect_prompt() {
        let b = Fake { authed: false, ..Default::default() };
        let resp = handle_request(
            &req(4, "tools/call", json!({ "name": "overview", "arguments": {} })),
            &b,
            "9.9.9",
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("isn't connected"));
    }

    #[tokio::test]
    async fn tool_call_forwards_and_passes_the_result_through() {
        let b = Fake {
            authed: true,
            call_result: Some(Ok(json!({ "content": [{ "type": "text", "text": "$42" }] }))),
            ..Default::default()
        };
        let resp = handle_request(
            &req(5, "tools/call", json!({ "name": "overview", "arguments": {} })),
            &b,
            "9.9.9",
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["content"][0]["text"], "$42");
    }

    #[tokio::test]
    async fn a_401_maps_to_a_repair_message() {
        let b = Fake {
            authed: true,
            call_result: Some(Err(ApiError { status: 401, body: None })),
            ..Default::default()
        };
        let resp = handle_request(
            &req(6, "tools/call", json!({ "name": "overview", "arguments": {} })),
            &b,
            "9.9.9",
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("401"));
    }

    #[tokio::test]
    async fn a_404_maps_to_a_stale_catalog_message() {
        let b = Fake {
            authed: true,
            call_result: Some(Err(ApiError { status: 404, body: None })),
            ..Default::default()
        };
        let resp = handle_request(
            &req(7, "tools/call", json!({ "name": "gone", "arguments": {} })),
            &b,
            "9.9.9",
        )
        .await
        .unwrap();
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("out of date"));
    }

    #[tokio::test]
    async fn eager_session_insights_kicks_a_scan_before_forwarding() {
        let b = Fake {
            authed: true,
            call_result: Some(Ok(json!({ "content": [] }))),
            ..Default::default()
        };
        let _ = handle_request(
            &req(
                8,
                "tools/call",
                json!({ "name": "session_insights", "arguments": { "session_ids": ["s1", "s2"], "eager": true } }),
            ),
            &b,
            "9.9.9",
        )
        .await;
        assert_eq!(*b.eager_ids.borrow(), vec!["s1".to_string(), "s2".to_string()]);
    }

    #[tokio::test]
    async fn a_notification_gets_no_reply() {
        let b = Fake::default();
        let notif = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_request(&notif, &b, "9.9.9").await.is_none());
    }

    #[tokio::test]
    async fn unknown_method_is_a_jsonrpc_error() {
        let b = Fake::default();
        let resp = handle_request(&req(9, "nope/nope", json!({})), &b, "9.9.9")
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn resources_read_serves_the_widget_html() {
        let b = Fake::default();
        let resp = handle_request(
            &req(10, "resources/read", json!({ "uri": WIDGET_URI })),
            &b,
            "9.9.9",
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["contents"][0]["mimeType"], "text/html;profile=mcp-app");
        assert!(resp["result"]["contents"][0]["text"].as_str().unwrap().contains("modelstat"));
    }
}
