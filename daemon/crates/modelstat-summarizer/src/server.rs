//! The protocol-v1 axum server (feature §10.4): `GET /healthz` +
//! `POST /v1/complete`, 1 MB body cap, 503 + Retry-After while (down)loading,
//! 500 on inference failure (status-only — the real cause is logged, never
//! echoed to the client, §21.13). The engine holds no identity and never logs
//! prompt/completion bodies.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use modelstat_llm::{CompleteOutcome, Engine, GenParams};
use modelstat_sumclient::{
    CompleteRequest, CompleteResponse, EngineError, HealthResponse, MODEL_ID, PROTOCOL_VERSION,
};

/// The bare semver reported by `/healthz` (matches the golden fixture; the
/// collector compares it to its own semver for skew).
pub const HEALTH_VERSION: &str = env!("CARGO_PKG_VERSION");

const MAX_BODY: usize = 1024 * 1024; // 1 MB (§10.4)
const RETRY_AFTER_SECS: &str = "2";

/// Build the protocol-v1 router over a shared [`Engine`].
pub fn router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/v1/complete",
            post(complete).layer(DefaultBodyLimit::max(MAX_BODY)),
        )
        .with_state(engine)
}

async fn healthz(State(engine): State<Arc<Engine>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        protocol: PROTOCOL_VERSION,
        version: HEALTH_VERSION.to_string(),
        model: MODEL_ID.to_string(),
        model_loaded: engine.is_loaded(),
        backend: engine.backend_name().to_string(),
    })
}

async fn complete(State(engine): State<Arc<Engine>>, body: Json<CompleteRequest>) -> Response {
    let req = body.0;
    let params = GenParams {
        system: req.system,
        user: req.user,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        top_k: req.top_k,
    };
    match engine.complete(params).await {
        CompleteOutcome::Ready(text) => {
            (StatusCode::OK, Json(CompleteResponse { text })).into_response()
        }
        CompleteOutcome::Loading => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, RETRY_AFTER_SECS)],
            Json(EngineError {
                error: "model_loading".to_string(),
            }),
        )
            .into_response(),
        CompleteOutcome::Failed(cause) => {
            // Fail loud in the engine's own logs; status-only to the client.
            modelstat_log::log_error!("summarizer: inference failed: {cause}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(EngineError {
                    error: "inference_failed".to_string(),
                }),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use modelstat_llm::{EngineConfig, MockBackend};
    use modelstat_sumclient::SummarizerClient;

    static NONCE: AtomicU64 = AtomicU64::new(0);

    /// Spawn the protocol server over an engine with `backend`, returning its
    /// base URL. A dummy model file makes the engine skip the download.
    async fn spawn(backend: MockBackend) -> String {
        let dir = std::env::temp_dir().join(format!(
            "modelstat-srv-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("model.gguf");
        std::fs::write(&model, b"pretend gguf").unwrap();
        let cfg = EngineConfig {
            bind: "127.0.0.1".into(),
            port: 0,
            model_path: model,
            context: 4096,
            parallel: 1,
            idle_unload_ms: 0,
        };
        let engine = Arc::new(Engine::new(backend, cfg));
        let app = router(engine);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn req() -> CompleteRequest {
        CompleteRequest {
            system: "sys".into(),
            user: "hello there".into(),
            temperature: 0.2,
            max_tokens: 1024,
            top_k: Some(3),
        }
    }

    #[tokio::test]
    async fn healthz_reports_protocol_v1() {
        let base = spawn(MockBackend::ready()).await;
        let client = SummarizerClient::with_timeout(&base, Duration::from_secs(5));
        let h = client.healthz().await.unwrap();
        assert_eq!(h.protocol, PROTOCOL_VERSION);
        assert_eq!(h.model, MODEL_ID);
        assert_eq!(h.version, HEALTH_VERSION);
        assert!(!h.model_loaded); // lazy — nothing loaded until first complete
    }

    #[tokio::test]
    async fn complete_loads_lazily_then_returns_stripped_text() {
        let base = spawn(MockBackend::ready()).await;
        let client = SummarizerClient::with_timeout(&base, Duration::from_secs(5));
        // The first call gets 503 (loading) and the client retries through it.
        let text = client.complete(&req()).await.unwrap();
        assert_eq!(text, "a concise redacted summary"); // <think> stripped by the engine
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let base = spawn(MockBackend::ready()).await;
        let big = "x".repeat(2 * 1024 * 1024); // 2 MB > the 1 MB cap
                                               // The body-limit layer refuses the upload. Depending on timing the client
                                               // either reads the 413 response cleanly, or — if the server sends 413 and
                                               // closes the connection before we finish writing all 2 MB — reqwest
                                               // surfaces the reset as a transport error instead. BOTH mean the oversized
                                               // body was rejected (never accepted + processed), which is what this test
                                               // guards. Asserting *only* 413 made it flaky (~1/3) on the RST race; a 200
                                               // or a timeout would still (correctly) fail here.
        match reqwest::Client::new()
            .post(format!("{base}/v1/complete"))
            .header("content-type", "application/json")
            .body(big)
            .send()
            .await
        {
            Ok(resp) => assert_eq!(
                resp.status().as_u16(),
                413,
                "a response to the oversized body must be 413, got {}",
                resp.status()
            ),
            Err(e) => assert!(
                !e.is_timeout(),
                "oversized body must be rejected (413 or connection reset), not time out: {e}"
            ),
        }
    }
}
