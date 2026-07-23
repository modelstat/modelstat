//! The MCP bridge runtime (§12): the real HTTP-backed [`McpBackend`] + the auth
//! chain + the stdin/stdout JSON-RPC loop that [`run_bridge`] drives. The pure
//! dispatch lives in [`crate::bridge`]; this is the I/O.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use modelstat_ingest::{home_path, modelstat_home, Config};

use crate::bridge::{handle_request, log, ApiError, McpBackend};

/// The loopback control port the eager pre-scan kicks (must match the daemon's).
const CONTROL_PORT: u16 = 4319;
/// Live tools/list fetch timeout (§12).
const TOOLS_TIMEOUT: Duration = Duration::from_millis(1500);
/// Eager pre-scan cap (§12).
const EAGER_TIMEOUT: Duration = Duration::from_secs(15);

/// The auth chain (§12): `MODELSTAT_TOKEN` env → in-process identity read →
/// `MODELSTAT_STATE_FILE`/`~/.modelstat/identity.json` (via [`Config`]) → our own
/// `mcp-auth.json`. Returns the bearer, or `None` (the daemon-less browser-claim
/// is served by the standalone npm package, §12).
fn resolve_bearer(config: &Config) -> Option<String> {
    if let Ok(tok) = std::env::var("MODELSTAT_TOKEN") {
        let tok = tok.trim();
        if !tok.is_empty() {
            return Some(tok.to_string());
        }
    }
    if let Some(b) = config.bearer() {
        return Some(b);
    }
    // Our own persisted bearer (from a prior browser claim by the npm package).
    let raw = std::fs::read_to_string(mcp_auth_path()).ok()?;
    serde_json::from_str::<Value>(&raw)
        .ok()?
        .get("token")?
        .as_str()
        .map(String::from)
}

fn mcp_auth_path() -> PathBuf {
    home_path("mcp-auth.json")
}
fn tools_cache_path() -> PathBuf {
    home_path("mcp-tools-cache.json")
}

/// The production backend: forwards to `/v1/mcp/*` under the resolved bearer.
pub struct HttpBackend {
    client: reqwest::Client,
    api_url: String,
    bearer: Option<String>,
}

impl HttpBackend {
    pub fn new(config: &Config) -> Self {
        HttpBackend {
            client: reqwest::Client::new(),
            api_url: config.api_url().trim_end_matches('/').to_string(),
            bearer: resolve_bearer(config),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api_url)
    }
}

/// Read the cached tool list (`{tools:[…]}`) from disk.
fn read_tools_cache() -> Option<Vec<Value>> {
    let raw = std::fs::read_to_string(tools_cache_path()).ok()?;
    serde_json::from_str::<Value>(&raw)
        .ok()?
        .get("tools")?
        .as_array()
        .cloned()
}

/// Atomically write the tool cache (tmp + rename).
fn write_tools_cache(tools: &[Value]) {
    let path = tools_cache_path();
    let _ = std::fs::create_dir_all(modelstat_home());
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    if std::fs::write(&tmp, json!({ "tools": tools }).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

impl McpBackend for HttpBackend {
    async fn list_tools(&self) -> Option<Vec<Value>> {
        let Some(bearer) = &self.bearer else {
            return read_tools_cache();
        };
        let live = self
            .client
            .get(self.url("/v1/mcp/tools"))
            .bearer_auth(bearer)
            .timeout(TOOLS_TIMEOUT)
            .send()
            .await
            .ok()
            .filter(|r| r.status().is_success());
        if let Some(resp) = live {
            if let Ok(body) = resp.json::<Value>().await {
                if let Some(tools) = body.get("tools").and_then(Value::as_array) {
                    write_tools_cache(tools);
                    log(&format!("tools=remote count={}", tools.len()));
                    return Some(tools.clone());
                }
            }
        }
        // Live failed → cache (dispatch falls back to static on None).
        match read_tools_cache() {
            Some(t) => {
                log(&format!("tools=cached count={}", t.len()));
                Some(t)
            }
            None => {
                log("tools=static (remote + cache unavailable)");
                None
            }
        }
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<Value, ApiError> {
        let bearer = self.bearer.as_deref().unwrap_or("");
        // No client timeout (§12): a tool call may be a long server operation.
        let resp = self
            .client
            .post(self.url("/v1/mcp/call"))
            .bearer_auth(bearer)
            .json(&json!({ "name": name, "arguments": args }))
            .send()
            .await
            .map_err(|e| ApiError {
                status: 0,
                body: Some(e.to_string()),
            })?;
        let status = resp.status();
        if status.is_success() {
            resp.json::<Value>().await.map_err(|e| ApiError {
                status: 0,
                body: Some(e.to_string()),
            })
        } else {
            let body = resp.text().await.ok();
            Err(ApiError {
                status: status.as_u16(),
                body,
            })
        }
    }

    async fn list_prompts(&self) -> Vec<Value> {
        let bearer = self.bearer.as_deref().unwrap_or("");
        let live = self
            .client
            .get(self.url("/v1/mcp/prompts"))
            .bearer_auth(bearer)
            .timeout(TOOLS_TIMEOUT)
            .send()
            .await
            .ok()
            .filter(|r| r.status().is_success());
        if let Some(resp) = live {
            if let Ok(body) = resp.json::<Value>().await {
                if let Some(prompts) = body.get("prompts").and_then(Value::as_array) {
                    return prompts.clone();
                }
            }
        }
        Vec::new()
    }

    async fn get_prompt(&self, name: &str, args: Value) -> Result<Value, ApiError> {
        let bearer = self.bearer.as_deref().unwrap_or("");
        let resp = self
            .client
            .post(self.url("/v1/mcp/prompt"))
            .bearer_auth(bearer)
            .json(&json!({ "name": name, "arguments": args }))
            .send()
            .await
            .map_err(|e| ApiError {
                status: 0,
                body: Some(e.to_string()),
            })?;
        let status = resp.status();
        if status.is_success() {
            resp.json::<Value>().await.map_err(|e| ApiError {
                status: 0,
                body: Some(e.to_string()),
            })
        } else {
            Err(ApiError {
                status: status.as_u16(),
                body: resp.text().await.ok(),
            })
        }
    }

    async fn eager_scan(&self, session_ids: Vec<String>) {
        // Best-effort loopback kick; a missing daemon / slow scan never blocks.
        let r = self
            .client
            .post(format!("http://127.0.0.1:{CONTROL_PORT}/v1/control/scan"))
            .json(&json!({ "session_ids": session_ids, "wait": true }))
            .timeout(EAGER_TIMEOUT)
            .send()
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                log("eager: daemon force-scanned the session")
            }
            Ok(_) => log("eager: daemon returned non-2xx — proceeding"),
            Err(_) => log("eager: no local daemon on the control port — proceeding"),
        }
    }

    fn has_auth(&self) -> bool {
        self.bearer.is_some()
    }
}

/// Run the stdio MCP bridge until stdin closes. stdout carries the JSON-RPC
/// protocol exclusively (one message per line); logs go to stderr. Port of
/// `index.ts::main`.
pub async fn run_bridge(version: &'static str) {
    let config = Config::load(version);
    let backend = HttpBackend::new(&config);
    log(&format!(
        "ready (auth={})",
        if backend.has_auth() {
            "present"
        } else {
            "none"
        }
    ));

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                log(&format!("dropping unparseable line: {e}"));
                continue;
            }
        };
        if let Some(resp) = handle_request(&req, &backend, version).await {
            if let Ok(mut out) = serde_json::to_string(&resp) {
                out.push('\n');
                if stdout.write_all(out.as_bytes()).await.is_err() {
                    break;
                }
                let _ = stdout.flush().await;
            }
        }
    }
}
