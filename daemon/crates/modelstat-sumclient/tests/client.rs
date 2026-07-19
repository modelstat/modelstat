//! Integration tests for the protocol-v1 client against a fake engine (plan §6
//! test double — modes: down / 503-loading / slow / garbage). The fake is an
//! in-process axum server bound to an ephemeral loopback port.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use modelstat_sumclient::{SumError, SummarizerClient};
use serde_json::{json, Value};

#[derive(Clone, Copy)]
enum Mode {
    Healthy,
    Garbage,
    Always400,
    Always503,
    /// Return 503 for the first N `/v1/complete` calls, then 200.
    FailThenOk(usize),
}

struct FakeState {
    mode: Mode,
    calls: AtomicUsize,
}

async fn healthz() -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "protocol": 1,
        "version": "daemon-9.9.9",
        "model": "qwen3.5-4b-q4_k_m",
        "model_loaded": true,
        "backend": "cpu"
    }))
}

async fn complete(State(st): State<Arc<FakeState>>, _body: Json<Value>) -> axum::response::Response {
    let n = st.calls.fetch_add(1, Ordering::SeqCst);
    let ok = || (StatusCode::OK, Json(json!({ "text": "summary text" }))).into_response();
    let loading = || {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "0")],
            Json(json!({ "error": "model_loading" })),
        )
            .into_response()
    };
    match st.mode {
        Mode::Healthy => ok(),
        Mode::Garbage => (StatusCode::OK, "not json {{{").into_response(),
        Mode::Always400 => (StatusCode::BAD_REQUEST, Json(json!({ "error": "bad" }))).into_response(),
        Mode::Always503 => loading(),
        Mode::FailThenOk(k) => {
            if n < k {
                loading()
            } else {
                ok()
            }
        }
    }
}

/// Spawn the fake engine, returning its base URL.
async fn spawn(mode: Mode) -> String {
    let state = Arc::new(FakeState {
        mode,
        calls: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/complete", post(complete))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn req() -> modelstat_sumclient::CompleteRequest {
    modelstat_sumclient::CompleteRequest {
        system: "sys".into(),
        user: "usr".into(),
        temperature: 0.2,
        max_tokens: 1024,
        top_k: Some(3),
    }
}

#[tokio::test]
async fn healthy_engine_completes() {
    let base = spawn(Mode::Healthy).await;
    let client = SummarizerClient::with_timeout(base, Duration::from_secs(5));
    let text = client.complete(&req()).await.unwrap();
    assert_eq!(text, "summary text");
}

#[tokio::test]
async fn healthz_parses() {
    let base = spawn(Mode::Healthy).await;
    let client = SummarizerClient::with_timeout(base, Duration::from_secs(5));
    let health = client.healthz().await.unwrap();
    assert_eq!(health.protocol, 1);
    assert_eq!(health.model, "qwen3.5-4b-q4_k_m");
    // The fake reports a different version → skew surfaces for status.
    assert!(client.check_skew(&health, "daemon-1.0.0").is_some());
}

#[tokio::test]
async fn loading_503_then_recovers() {
    // One 503 (Retry-After: 0 → 250ms floor), then success — within 3 attempts.
    let base = spawn(Mode::FailThenOk(1)).await;
    let client = SummarizerClient::with_timeout(base, Duration::from_secs(5));
    let text = client.complete(&req()).await.unwrap();
    assert_eq!(text, "summary text");
}

#[tokio::test]
async fn persistent_503_fails_after_three_attempts() {
    let base = spawn(Mode::Always503).await;
    let client = SummarizerClient::with_timeout(base, Duration::from_secs(5));
    let err = client.complete(&req()).await.unwrap_err();
    assert_eq!(err, SumError::Http(503));
}

#[tokio::test]
async fn non_retryable_4xx_fails_immediately() {
    let base = spawn(Mode::Always400).await;
    let client = SummarizerClient::with_timeout(base, Duration::from_secs(5));
    let err = client.complete(&req()).await.unwrap_err();
    // A single attempt (no retry) — the 400 is terminal.
    assert_eq!(err, SumError::Http(400));
}

#[tokio::test]
async fn garbage_output_is_a_decode_error() {
    let base = spawn(Mode::Garbage).await;
    let client = SummarizerClient::with_timeout(base, Duration::from_secs(5));
    let err = client.complete(&req()).await.unwrap_err();
    assert!(matches!(err, SumError::Decode(_)), "got {err:?}");
}

#[tokio::test]
async fn engine_down_is_a_transport_error() {
    // Bind then release the port so nothing is listening → connection refused.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = SummarizerClient::with_timeout(format!("http://{addr}"), Duration::from_secs(2));
    let err = client.complete(&req()).await.unwrap_err();
    assert!(
        matches!(err, SumError::Transport(_) | SumError::Timeout),
        "got {err:?}"
    );
}
