//! A fake device-API server (plan §6 "fake … server harness") implementing the
//! three device endpoints the M1 collector calls — `POST /v1/tokens`,
//! `GET /v1/devices/me`, `POST /v1/devices/{id}/heartbeat` — with the SAME
//! `{data:…}` envelope + field names as the real core server (verified against
//! `core/rust/crates/api/src/devices.rs`), plus test-control endpoints to revoke
//! a secret (simulate 401 revocation) and mark a device claimed.
//!
//! It reproduces the one behaviour the M1 acceptance criteria hinge on: dedupe
//! on `fingerprint.machine_id` — a re-register of a known machine returns the
//! SAME `device_id` with a fresh secret and `re_registered: true`. That is what
//! makes fresh-register / reuse / `--fresh` all converge onto one device row.
//!
//! Docker isn't available in this environment to stand up the real Postgres/
//! ClickHouse/MinIO stack, so this harness is how the five scenarios are proven
//! deterministically; the same flows run against a real `$DAEMON_API_URL` via
//! `scripts/e2e-m1.sh`.
//!
//! Shared by the `e2e_m1` integration test and the `fake_device_server` example
//! (via `#[path = …]`), so `#[allow(dead_code)]` covers items only one uses.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

/// Monotonic id/secret source (unique across re-registers within a run).
static COUNTER: AtomicU64 = AtomicU64::new(1);

fn next() -> u64 {
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

struct Device {
    device_uuid: String,
    /// Currently-valid secrets. Revocation empties this (the row survives so a
    /// re-register converges — matching the real additive-secret model).
    secrets: HashSet<String>,
    claimed: bool,
    user_id: Option<String>,
    claim_code: String,
}

#[derive(Default)]
struct FakeState {
    /// machine_id → device_id (the dedupe anchor, the partial unique index).
    by_machine: HashMap<String, String>,
    devices: HashMap<String, Device>,
    /// valid secret → device_id (auth lookup).
    secret_index: HashMap<String, String>,
    base_url: String,
}

type Shared = Arc<Mutex<FakeState>>;

/// A running fake server. Dropping it aborts the serve task.
pub struct FakeServer {
    pub base_url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Bind `127.0.0.1:0`, learn the port, then serve. Returns once the listener is
/// bound so callers can hit it immediately.
pub async fn spawn() -> FakeServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");
    let state: Shared = Arc::new(Mutex::new(FakeState {
        base_url: base_url.clone(),
        ..Default::default()
    }));
    let app = router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    FakeServer { base_url, handle }
}

/// Build the router on an already-bound listener at `bind` (host:port). Used by
/// the standalone example, which needs a fixed address.
pub async fn serve_on(bind: &str) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    let base_url = format!("http://{addr}");
    let state: Shared = Arc::new(Mutex::new(FakeState {
        base_url: base_url.clone(),
        ..Default::default()
    }));
    println!("fake-device-server listening on {base_url}");
    axum::serve(listener, router(state)).await
}

fn router(state: Shared) -> Router {
    Router::new()
        .route("/v1/tokens", post(register))
        .route("/v1/devices/me", get(devices_me))
        .route("/v1/devices/{id}/heartbeat", post(heartbeat))
        .route("/_control/revoke", post(control_revoke))
        .route("/_control/claim", post(control_claim))
        .with_state(state)
}

fn mint_secret() -> String {
    format!("ds_live_{:064x}", next())
}

/// `{data: payload}` — the agentic success envelope the real server wraps every
/// device response in.
fn envelope(payload: Value) -> Json<Value> {
    Json(json!({ "data": payload, "summary": "ok" }))
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// `POST /v1/tokens` — register/self-register, deduped on `machine_id`.
async fn register(State(st): State<Shared>, Json(body): Json<Value>) -> Response {
    let device_uuid = body
        .get("device_uuid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let machine_id = body
        .get("fingerprint")
        .and_then(|f| f.get("machine_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut s = st.lock().unwrap();
    let base = s.base_url.clone();
    let secret = mint_secret();
    let secret_prefix = secret[..12].to_string();

    let (device_id, re_registered) = match s.by_machine.get(&machine_id).cloned() {
        Some(existing) => (existing, true),
        None => {
            let id = format!("dev_{:012x}", next());
            let claim_code = format!("code-{:08x}", next());
            s.by_machine.insert(machine_id.clone(), id.clone());
            s.devices.insert(
                id.clone(),
                Device {
                    device_uuid: device_uuid.clone(),
                    secrets: HashSet::new(),
                    claimed: false,
                    user_id: None,
                    claim_code,
                },
            );
            (id, false)
        }
    };

    // Additive: the fresh secret is valid alongside any others.
    s.secret_index.insert(secret.clone(), device_id.clone());
    let dev = s.devices.get_mut(&device_id).unwrap();
    dev.secrets.insert(secret.clone());
    let claimed = dev.claimed;
    let user_id = dev.user_id.clone();
    let claim_code = dev.claim_code.clone();

    let (claim_code_out, claim_url_out) = if claimed {
        (Value::Null, Value::Null)
    } else {
        (
            json!(claim_code),
            json!(format!("{base}/device/{claim_code}")),
        )
    };

    envelope(json!({
        "device_id": device_id,
        "device_uuid": device_uuid,
        "device_secret": secret,
        "secret_prefix": secret_prefix,
        "claim_code": claim_code_out,
        "claim_url": claim_url_out,
        "status": if claimed { "claimed" } else { "unclaimed" },
        "user_id": user_id,
        "re_registered": re_registered,
    }))
    .into_response()
}

/// `GET /v1/devices/me` — auth by device secret; unknown/revoked secret → 401.
async fn devices_me(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let Some(secret) = bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, "no bearer").into_response();
    };
    let s = st.lock().unwrap();
    let Some(device_id) = s.secret_index.get(&secret).cloned() else {
        return (StatusCode::UNAUTHORIZED, "device_secret not recognised").into_response();
    };
    let dev = s.devices.get(&device_id).unwrap();
    let (claim_code, claim_url) = if dev.claimed {
        (Value::Null, Value::Null)
    } else {
        (
            json!(dev.claim_code),
            json!(format!("{}/device/{}", s.base_url, dev.claim_code)),
        )
    };
    envelope(json!({
        "device_id": device_id,
        "device_uuid": dev.device_uuid,
        "self_registered": true,
        "status": if dev.claimed { "claimed" } else { "unclaimed" },
        "claim_code": claim_code,
        "claim_url": claim_url,
        "user_id": dev.user_id,
    }))
    .into_response()
}

/// `POST /v1/devices/{id}/heartbeat` — auth by device secret; returns a
/// `daemon_release` verdict like the real server.
async fn heartbeat(
    State(st): State<Shared>,
    Path(_id): Path<String>,
    headers: HeaderMap,
    _body: Option<Json<Value>>,
) -> Response {
    let Some(secret) = bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, "no bearer").into_response();
    };
    let s = st.lock().unwrap();
    if !s.secret_index.contains_key(&secret) {
        return (StatusCode::UNAUTHORIZED, "device_secret not recognised").into_response();
    }
    envelope(json!({
        "daemon_release": { "verdict": "ok", "min": null, "latest": null },
        "server_time": "2026-07-15T00:00:00.000Z",
        "installations_upserted": 0,
        "identities_upserted": 0,
    }))
    .into_response()
}

/// Test control: revoke ALL of a device's secrets (the row survives so a
/// re-register converges) — simulates a server-side revocation. Body:
/// `{ "device_id": "dev_…" }`.
async fn control_revoke(State(st): State<Shared>, Json(body): Json<Value>) -> Response {
    let device_id = body
        .get("device_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut s = st.lock().unwrap();
    if let Some(dev) = s.devices.get_mut(&device_id) {
        let secrets: Vec<String> = dev.secrets.drain().collect();
        for sec in secrets {
            s.secret_index.remove(&sec);
        }
    }
    (StatusCode::OK, "revoked").into_response()
}

/// Test control: mark a device claimed. Body:
/// `{ "device_id": "dev_…", "user_id": "user_…" }`.
async fn control_claim(State(st): State<Shared>, Json(body): Json<Value>) -> Response {
    let device_id = body
        .get("device_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let user_id = body
        .get("user_id")
        .and_then(Value::as_str)
        .unwrap_or("user_1")
        .to_string();
    let mut s = st.lock().unwrap();
    if let Some(dev) = s.devices.get_mut(&device_id) {
        dev.claimed = true;
        dev.user_id = Some(user_id);
    }
    (StatusCode::OK, "claimed").into_response()
}
