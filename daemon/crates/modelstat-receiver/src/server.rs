//! The loopback ingest server — a port of the axum-equivalent HTTP layer of
//! `apps/daemon/src/receiver.ts` (`startLocalIngestReceiver` + `handle` +
//! `handleControlScan` + `createControlRunner` + `isAllowedTranscriptFile`).
//!
//! Binds `127.0.0.1:4319` (best-effort — a busy port disables the SDK path with
//! a warning, never crashes the daemon's file-scan duty). Routes:
//!   - `GET  /healthz` → `{ ok: true }`
//!   - `POST /v1/ingest` | `/v1/ingest/raw` → validate + durably enqueue (the
//!     daemon redacts + summarises on its tick regardless of which door was used)
//!   - `POST /v1/control/scan` → force-scan (when a handler is wired), serialized
//!   - anything else → 404
//!
//! The request LOGIC (`parse_batch`/`enqueue`) lives in [`crate::ingest`]; this
//! module is the transport + routing + control plane.

use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::ingest::{enqueue, parse_batch};
use crate::queue::FileQueueStore;

/// Default loopback port — must match the SDKs' `DEFAULT_DAEMON_URL`.
pub const DEFAULT_LOCAL_INGEST_PORT: u16 = 4319;
/// Cap on one ingest POST body (a generous abuse guard, not a tuning knob).
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Control-scan bodies are tiny (a few ids); a small cap is plenty.
const CONTROL_MAX_BODY_BYTES: usize = 64 * 1024;

/// What `POST /v1/control/scan` asks the daemon to force-scan.
pub struct ControlTarget {
    pub session_ids: Option<Vec<String>>,
    pub file: Option<String>,
}

/// The injected force-scan worker (daemon-main provides it). A boxed future keeps
/// the receiver a concrete, non-generic type (mirrors the pipeline's
/// `LinkExtractor` seam). Resolves when the scan + insight refresh have finished.
pub type ControlScanHandler =
    Arc<dyn Fn(ControlTarget) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Serializes force-scan runs — a burst of control posts never runs two scans at
/// once; each waits for the previous to drain. Port of `createControlRunner`,
/// which CHAINS every target (not coalesce-drop — a requested session must run).
pub struct ControlRunner {
    handler: ControlScanHandler,
    lock: tokio::sync::Mutex<()>,
}

impl ControlRunner {
    pub fn new(handler: ControlScanHandler) -> Self {
        ControlRunner {
            handler,
            lock: tokio::sync::Mutex::new(()),
        }
    }
    /// Run one target after any in-flight scan drains. The caller awaits this for
    /// a `wait:true` request, or spawns it fire-and-forget.
    pub async fn run(&self, target: ControlTarget) {
        let _guard = self.lock.lock().await;
        (self.handler)(target).await;
    }
}

#[derive(Clone)]
struct AppState {
    queue: Arc<FileQueueStore>,
    control: Option<Arc<ControlRunner>>,
}

/// Build the router with explicit body caps (production uses [`MAX_BODY_BYTES`] /
/// [`CONTROL_MAX_BODY_BYTES`]; tests pass small caps to exercise the limit).
fn build_router(state: AppState, max_ingest: usize, max_control: usize) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/v1/ingest",
            post(ingest).layer(DefaultBodyLimit::max(max_ingest)),
        )
        .route(
            "/v1/ingest/raw",
            post(ingest).layer(DefaultBodyLimit::max(max_ingest)),
        )
        .route(
            "/v1/control/scan",
            post(control_scan).layer(DefaultBodyLimit::max(max_control)),
        )
        .fallback(not_found)
        .with_state(state)
}

async fn healthz() -> Response {
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

async fn ingest(State(st): State<AppState>, body: Bytes) -> Response {
    match parse_batch(&body) {
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
        Ok(batch) => match enqueue(&*st.queue, &batch).await {
            Ok(accepted) => (
                StatusCode::OK,
                Json(json!({ "accepted": accepted, "queued": true })),
            )
                .into_response(),
            Err(e) => {
                modelstat_log::log_error!("local ingest enqueue failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "enqueue failed" })),
                )
                    .into_response()
            }
        },
    }
}

#[derive(Deserialize)]
struct ControlScanRequest {
    #[serde(default)]
    session_ids: Option<Vec<String>>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    wait: Option<bool>,
}

async fn control_scan(State(st): State<AppState>, body: Bytes) -> Response {
    let Some(control) = st.control.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "control scan unavailable" })),
        )
            .into_response();
    };
    // An empty body is a bare "scan the newest transcript" request.
    let raw: &[u8] = if body.is_empty() { b"{}" } else { &body };
    let req: ControlScanRequest = match serde_json::from_slice(raw) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid json" })),
            )
                .into_response()
        }
    };
    // Keep only non-empty session ids.
    let session_ids = req
        .session_ids
        .map(|v| v.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>());
    let mut file = None;
    if let Some(f) = req.file {
        if !is_allowed_transcript_file(&f) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "file must be under ~/.claude/projects or ~/.codex/sessions" })),
            )
                .into_response();
        }
        file = Some(f);
    }
    let target = ControlTarget { session_ids, file };
    if req.wait == Some(true) {
        control.run(target).await;
        (StatusCode::OK, Json(json!({ "ok": true, "scanned": true }))).into_response()
    } else {
        // Fire-and-forget: don't let the request block on the scan.
        tokio::spawn(async move { control.run(target).await });
        (StatusCode::OK, Json(json!({ "ok": true, "started": true }))).into_response()
    }
}

/// Lexically resolve `.`/`..` WITHOUT touching the filesystem (unlike
/// `std::fs::canonicalize`, which requires the path to exist) — so an allowlist
/// check works for a brand-new file mid-write, and `..` can't traverse out.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    // Rebuild the path from the surviving components (`push` treats a RootDir's
    // "/" as an absolute reset, so an absolute input stays absolute).
    let mut norm = PathBuf::new();
    for c in &out {
        norm.push(c.as_os_str());
    }
    norm
}

/// True when `file` resolves to a path at or strictly inside one of `roots` —
/// component-wise (so `/a/bc` does NOT match root `/a/b`). Pure core of
/// [`is_allowed_transcript_file`], testable with fixed roots.
fn is_allowed_under(file: &str, roots: &[PathBuf]) -> bool {
    let target = lexical_normalize(Path::new(file));
    roots.iter().any(|root| target.starts_with(root))
}

/// True when `file` lives under `~/.claude/projects` or `~/.codex/sessions` — the
/// same transcript roots the scanner discovers. Rejects anything else so a local
/// POST can't make the daemon parse (and summarise) an arbitrary file.
pub fn is_allowed_transcript_file(file: &str) -> bool {
    is_allowed_under(file, &transcript_roots())
}

fn transcript_roots() -> Vec<PathBuf> {
    home_dir()
        .map(|h| vec![h.join(".claude/projects"), h.join(".codex/sessions")])
        .unwrap_or_default()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|s| !s.is_empty()))
        .map(PathBuf::from)
}

/// A running loopback receiver. `abort` stops it (the daemon calls it on shutdown).
pub struct LocalIngestReceiver {
    pub port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl LocalIngestReceiver {
    pub fn abort(&self) {
        self.handle.abort();
    }
}

/// Start the loopback receiver. Best-effort: a busy port (a stale listener; the
/// singleton daemon lock already prevents a second daemon) disables the SDK path
/// with a warning and returns `None`, rather than crashing the daemon. `port = 0`
/// lets the OS assign a free port (used by tests). Port of `startLocalIngestReceiver`.
pub async fn start_local_ingest_receiver(
    queue: Arc<FileQueueStore>,
    port: u16,
    on_control_scan: Option<ControlScanHandler>,
) -> Option<LocalIngestReceiver> {
    let control = on_control_scan.map(|h| Arc::new(ControlRunner::new(h)));
    let app = build_router(
        AppState { queue, control },
        MAX_BODY_BYTES,
        CONTROL_MAX_BODY_BYTES,
    );
    let addr = format!("127.0.0.1:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            modelstat_log::log_error!(
                "local ingest receiver disabled — SDK local_daemon mode \
                 unavailable: {e} on {addr}"
            );
            return None;
        }
    };
    let bound_port = listener.local_addr().ok()?.port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Some(LocalIngestReceiver {
        port: bound_port,
        handle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::QueueStore;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Mutex;
    use tower::ServiceExt; // oneshot

    fn temp_queue(tag: &str) -> Arc<FileQueueStore> {
        let path = std::env::temp_dir().join(format!(
            "modelstat-srv-{}-{tag}/queue.json",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        Arc::new(FileQueueStore::new(path))
    }

    fn app(state: AppState) -> Router {
        build_router(state, MAX_BODY_BYTES, CONTROL_MAX_BODY_BYTES)
    }

    // Send one request through the router in-process (no TCP), returning the
    // status + parsed JSON body.
    async fn send(
        router: Router,
        method: &str,
        uri: &str,
        body: Vec<u8>,
    ) -> (StatusCode, serde_json::Value) {
        let resp = router
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    fn ok_event(id: &str) -> serde_json::Value {
        json!({
            "source_event_id": id, "ts": "2026-07-16T10:00:00.000Z", "kind": "message",
            "agent": "claude_code", "provider": "anthropic", "session_id": "s1",
            "pricing_mode": "api"
        })
    }

    #[tokio::test]
    async fn healthz_ok() {
        let st = AppState {
            queue: temp_queue("hz"),
            control: None,
        };
        let (status, body) = send(app(st), "GET", "/healthz", Vec::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], json!(true));
    }

    #[tokio::test]
    async fn ingest_enqueues_a_valid_batch() {
        let queue = temp_queue("ing");
        let st = AppState {
            queue: queue.clone(),
            control: None,
        };
        let body =
            serde_json::to_vec(&json!({ "events": [ok_event("e1"), ok_event("e2")] })).unwrap();
        let (status, resp) = send(app(st), "POST", "/v1/ingest", body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["accepted"], json!(2));
        assert_eq!(resp["queued"], json!(true));
        assert_eq!(queue.count_unsent().await, 2);
    }

    #[tokio::test]
    async fn ingest_raw_uses_the_same_handler() {
        let queue = temp_queue("raw");
        let st = AppState {
            queue: queue.clone(),
            control: None,
        };
        let body = serde_json::to_vec(&json!({ "events": [ok_event("e1")] })).unwrap();
        let (status, _) = send(app(st), "POST", "/v1/ingest/raw", body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(queue.count_unsent().await, 1);
    }

    #[tokio::test]
    async fn ingest_rejects_an_invalid_batch() {
        let st = AppState {
            queue: temp_queue("bad"),
            control: None,
        };
        let body = serde_json::to_vec(&json!({ "events": [] })).unwrap();
        let (status, resp) = send(app(st), "POST", "/v1/ingest", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(resp["error"].as_str().unwrap().contains("non-empty"));
    }

    #[tokio::test]
    async fn oversized_body_is_rejected_413() {
        // Tiny cap so we exercise the limit without a 16MB allocation.
        let st = AppState {
            queue: temp_queue("big"),
            control: None,
        };
        let router = build_router(st, 100, CONTROL_MAX_BODY_BYTES);
        let body = vec![b'x'; 500]; // > the 100-byte cap
        let (status, _) = send(router, "POST", "/v1/ingest", body).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn control_scan_is_503_without_a_handler() {
        let st = AppState {
            queue: temp_queue("noctl"),
            control: None,
        };
        let (status, _) = send(app(st), "POST", "/v1/control/scan", Vec::new()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn control_scan_wait_runs_the_handler() {
        let seen = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let seen2 = seen.clone();
        let handler: ControlScanHandler = Arc::new(move |t: ControlTarget| {
            let s = seen2.clone();
            Box::pin(async move {
                s.lock().unwrap().push(t.file);
            })
        });
        let st = AppState {
            queue: temp_queue("ctl"),
            control: Some(Arc::new(ControlRunner::new(handler))),
        };
        let body = serde_json::to_vec(&json!({ "wait": true })).unwrap();
        let (status, resp) = send(app(st), "POST", "/v1/control/scan", body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["scanned"], json!(true));
        // wait:true awaited the scan, so the handler definitely ran.
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn control_scan_rejects_a_file_outside_the_roots() {
        let handler: ControlScanHandler = Arc::new(|_t| Box::pin(async {}));
        let st = AppState {
            queue: temp_queue("badfile"),
            control: Some(Arc::new(ControlRunner::new(handler))),
        };
        let body = serde_json::to_vec(&json!({ "file": "/etc/passwd" })).unwrap();
        let (status, _) = send(app(st), "POST", "/v1/control/scan", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_path_is_404() {
        let st = AppState {
            queue: temp_queue("nf"),
            control: None,
        };
        let (status, _) = send(app(st), "GET", "/nope", Vec::new()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn allowlist_permits_under_root_and_defeats_traversal() {
        let roots = vec![PathBuf::from("/home/dev/.claude/projects")];
        // Inside the root → allowed.
        assert!(is_allowed_under(
            "/home/dev/.claude/projects/p/a.jsonl",
            &roots
        ));
        // The root itself → allowed.
        assert!(is_allowed_under("/home/dev/.claude/projects", &roots));
        // `..` traversal that escapes → rejected.
        assert!(!is_allowed_under(
            "/home/dev/.claude/projects/../../../etc/passwd",
            &roots
        ));
        // A sibling that shares a string prefix but not a path prefix → rejected.
        assert!(!is_allowed_under(
            "/home/dev/.claude/projects-evil/x",
            &roots
        ));
        // Outside entirely → rejected.
        assert!(!is_allowed_under("/etc/passwd", &roots));
    }
}
