//! The compiled-in static tool catalog (§12) — the 8-tool fallback used at
//! runtime only when BOTH the live `GET /v1/mcp/tools` fetch and the on-disk
//! cache are unavailable, and published for discovery. Schemas, enums, and limits
//! are byte-faithful to the TS `packages/mcp/src/index.ts` `TOOLS` array. The live
//! catalog fully replaces this once a fetch succeeds.

use serde_json::{json, Value};

/// Named time windows the range-aware tools accept.
pub const RANGES: [&str; 6] = ["today", "7d", "30d", "90d", "mtd", "ytd"];
/// Dimensions `explore` can group / stack by.
pub const DIMENSIONS: [&str; 9] = [
    "provider", "model", "tool", "day", "hour", "device", "identity", "session", "taxonomy",
];
/// Metrics `explore` can compute.
pub const METRICS: [&str; 10] = [
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
];

/// The shared `range` / `from` / `to` schema properties.
fn range_props() -> Value {
    json!({
        "range": {
            "type": "string",
            "enum": RANGES,
            "description": "Named time window (ignored when from/to given). Omit range AND from/to for all-time."
        },
        "from": { "type": "string", "description": "RFC3339 inclusive lower bound (overrides `range`)" },
        "to": { "type": "string", "description": "RFC3339 exclusive upper bound (overrides `range`)" }
    })
}

/// Merge `range_props()` into an existing properties object.
fn with_range(mut props: Value) -> Value {
    if let (Some(obj), Value::Object(rp)) = (props.as_object_mut(), range_props()) {
        for (k, v) in rp {
            obj.insert(k, v);
        }
    }
    props
}

/// The static 8-tool catalog.
pub fn static_tools() -> Vec<Value> {
    let taxonomy_filter = json!({
        "type": "array",
        "description": "AND-of-OR taxonomy node-id groups, e.g. [[projId],[debugId]] = tagged BOTH. A flat array [a,b] is one OR-group. Auto-expanded to subtrees server-side.",
        "items": { "oneOf": [ { "type": "string" }, { "type": "array", "items": { "type": "string" } } ] }
    });
    vec![
        json!({
            "name": "overview",
            "description": "Headline spend/usage for the account: effective cost, list-price cost, savings, total tokens, event count, distinct sessions, ROI (repos/PRs), and taxonomy roots. Start here for 'how much did I spend?'. Costs are exact decimal USD strings.",
            "inputSchema": { "type": "object", "properties": with_range(json!({})) }
        }),
        json!({
            "name": "explore",
            "description": "The analytics workhorse (event/segment grain): group-by (and optionally stack-by) any dimension, pick a metric, filter, get back cells + whole-set totals. Time series: group_by=day|hour. Leaderboards: group_by=model|tool|session|identity. The `taxonomy` filter takes AND-of-OR groups [[idA,idB],[idC]] (AND across groups, OR within; a flat array is one OR-group) — resolve ids first with find_taxonomy / find_projects. This is how you answer cross-cutting questions like 'total $ debugging the acme project': find both ids, then explore with taxonomy:[[proj],[debug]]. Costs are exact decimal USD strings.",
            "inputSchema": { "type": "object", "properties": with_range(json!({
                "group_by": { "type": "string", "enum": DIMENSIONS, "default": "day" },
                "stack_by": { "type": "string", "enum": DIMENSIONS, "description": "Optional second dimension; each cell carries `stack`." },
                "metric": { "type": "string", "enum": METRICS, "default": "cost" },
                "taxonomy": taxonomy_filter,
                "providers": { "type": "array", "items": { "type": "string" }, "description": "e.g. [\"anthropic\"]" },
                "models": { "type": "array", "items": { "type": "string" } },
                "tools": { "type": "array", "items": { "type": "string" }, "description": "e.g. [\"claude_code\"]" },
                "identities": { "type": "array", "items": { "type": "string" }, "description": "identity ids" },
                "session_ids": { "type": "array", "items": { "type": "string" } },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 50, "description": "Top-N cap on returned groups." }
            })) }
        }),
        json!({
            "name": "sessions",
            "description": "Search/list sessions, most recent first (cursor-paginated). Filter by taxonomy AND-of-OR groups, project, device, identity, free-text `q`, and range. Each row: session_id, tool, total tokens, effective cost. Resolve names to ids with find_* first.",
            "inputSchema": { "type": "object", "properties": with_range(json!({
                "q": { "type": "string", "description": "free-text match over session abstracts/metadata" },
                "taxonomy": taxonomy_filter,
                "identity_id": { "type": "string" },
                "device_id": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500 },
                "cursor": { "type": "string" }
            })) }
        }),
        json!({
            "name": "session_insights",
            "description": "Live per-session insights for the CURRENT session: total tokens, effective $ assigned, and the taxonomy nodes detected, plus a status (ready | analyzing | not_ingested). Pass every session id in the transcript chain (compactions/resumes are one logical conversation). Set eager:true to force-scan the session locally first (via a running daemon) and prioritise server enrichment — then re-call with eager:false every ~2.5s while status==\"analyzing\" (up to ~20s). Returns a formatted text summary + structuredContent for rendering.",
            "inputSchema": {
                "type": "object",
                "required": ["session_ids"],
                "properties": {
                    "session_ids": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 200, "description": "The session-id chain for one logical conversation." },
                    "eager": { "type": "boolean", "default": false, "description": "Force-scan the session on the local daemon first + prioritise server enrichment. Use once, then poll with eager:false." }
                }
            }
        }),
        json!({
            "name": "find_taxonomy",
            "description": "Resolve taxonomy node names → ids. Search by name, optionally scoped to a root_key (e.g. work_type). Returns [{id,name,path,root_key,…}]. Feed the ids into explore/sessions `taxonomy` filters.",
            "inputSchema": { "type": "object", "properties": {
                "q": { "type": "string", "description": "name search, e.g. \"debugging\"" },
                "root_key": { "type": "string", "description": "restrict to one taxonomy root" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
            } }
        }),
        json!({
            "name": "find_projects",
            "description": "List/search projects (the `workstreams` taxonomy root) → node ids, with spend. Returns [{id,name,slug,cost_usd,sessions}] where `id` is a taxonomy node id usable directly as a filter in explore/sessions.",
            "inputSchema": { "type": "object", "properties": with_range(json!({
                "q": { "type": "string", "description": "project name search" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
            })) }
        }),
        json!({
            "name": "find_people",
            "description": "Search provider-account identities → ids for the `identities` filter. Returns matching identities you can then pass to explore/sessions.",
            "inputSchema": { "type": "object", "properties": {
                "q": { "type": "string", "description": "name/email/handle search" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
            } }
        }),
        json!({
            "name": "assign_session",
            "description": "MUTATING: reassign a session's owner/identity.",
            "inputSchema": {
                "type": "object",
                "required": ["session_id", "target"],
                "properties": {
                    "session_id": { "type": "string" },
                    "target": { "type": "string", "description": "identity/owner to assign" }
                }
            }
        }),
    ]
}

/// The widget tool that gets the `ui://` resource tag stamped on it (§12).
pub const WIDGET_TOOL: &str = "session_insights";
/// The MCP-Apps widget resource URI.
pub const WIDGET_URI: &str = "ui://modelstat/session-insights";

/// Stamp the widget `_meta.ui.resourceUri` (+ the legacy flat key) onto the
/// `session_insights` tool in a catalog, regardless of source (§12).
pub fn with_widget_meta(mut tools: Vec<Value>) -> Vec<Value> {
    for tool in &mut tools {
        if tool.get("name").and_then(Value::as_str) == Some(WIDGET_TOOL) {
            if let Some(obj) = tool.as_object_mut() {
                obj.insert(
                    "_meta".to_string(),
                    json!({ "ui": { "resourceUri": WIDGET_URI }, "ui.resourceUri": WIDGET_URI }),
                );
            }
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_catalog_has_the_eight_named_tools_with_schemas() {
        let tools = static_tools();
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "overview",
                "explore",
                "sessions",
                "session_insights",
                "find_taxonomy",
                "find_projects",
                "find_people",
                "assign_session",
            ]
        );
        // Every tool carries an object inputSchema.
        for t in &tools {
            assert_eq!(t["inputSchema"]["type"], "object");
        }
        // explore exposes the metric enum + a limit cap of 500.
        let explore = &tools[1];
        assert_eq!(explore["inputSchema"]["properties"]["metric"]["default"], "cost");
        assert_eq!(explore["inputSchema"]["properties"]["limit"]["maximum"], 500);
        // Range props are merged into overview.
        assert!(tools[0]["inputSchema"]["properties"]["range"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("7d")));
        // session_insights requires session_ids.
        assert_eq!(tools[3]["inputSchema"]["required"][0], "session_ids");
    }

    #[test]
    fn widget_meta_is_stamped_only_on_session_insights() {
        let tagged = with_widget_meta(static_tools());
        for t in &tagged {
            let has_meta = t.get("_meta").is_some();
            assert_eq!(
                has_meta,
                t["name"] == "session_insights",
                "unexpected _meta on {}",
                t["name"]
            );
        }
        let si = tagged.iter().find(|t| t["name"] == "session_insights").unwrap();
        assert_eq!(si["_meta"]["ui"]["resourceUri"], WIDGET_URI);
    }
}
