//! M4 IngestClient acceptance (feature §17.1/§17.2), driven end-to-end through
//! the REAL `DeviceApi::upload_batch` against an in-process scriptable ingest
//! server. Proves the upload WIRING:
//!
//!   · 2xx        → Commit, receipt parsed, bearer + summarizer_mode on the wire
//!   · raw = true → hits `/v1/ingest/raw`
//!
//! The never-drop DECISION (400/404/413/422/429/5xx → HOLD, never a permanent
//! drop; feature §21.2) is proven exhaustively by the pure `classify_status`
//! unit tests in `src/ingest.rs`. The end-to-end never-drop-ACROSS-CYCLES
//! behaviour — a Hold leaving the file cursor un-advanced so the same batch
//! re-ships next scan — is a scan-loop property, tested in M4's scan orchestrator.
//! (A real multi-attempt e2e here would burn the 1+2.5+5s backoff ladder in wall
//! clock, and `start_paused` can't help: it auto-advances virtual time through
//! reqwest's own request timeout during the real localhost round-trip.)
//!
//! One test function (no intra-binary `MODELSTAT_HOME`/`DAEMON_API_URL` races,
//! like `e2e_m1`).

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use modelstat_ingest::upload_gate::{MIN_CONCURRENCY, START_CONCURRENCY, WINS_TO_GROW};
use modelstat_ingest::{save_identity, Config, DeviceApi, DeviceIdentity, UploadResult};
use modelstat_wire::IngestBatch;
use serde_json::{json, Value};

/// Records the last request an ingest route received so the test can assert
/// path / auth / body. Every request is accepted (2xx + receipt).
#[derive(Default)]
struct Recorder {
    last_path: String,
    last_auth: Option<String>,
    last_body: Option<Value>,
    /// While set, the next request is answered `429` and the count decremented —
    /// how the test makes the server push back exactly once.
    throttle_next: usize,
}
type Shared = Arc<Mutex<Recorder>>;

fn record(st: &Shared, path: &str, headers: &HeaderMap, body: Value) -> Response {
    let mut s = st.lock().unwrap();
    s.last_path = path.to_string();
    s.last_auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    s.last_body = Some(body);
    if s.throttle_next > 0 {
        s.throttle_next -= 1;
        // `Retry-After: 0` so the retry costs only the backoff ladder's first
        // rung, not a scripted wait on top of it.
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "0")],
            Json(json!({ "error": "over fair share" })),
        )
            .into_response();
    }
    Json(json!({
        "accepted": 3,
        "new_sessions": 1,
        "updated_sessions": 2,
        "batch_id": "batch_srv",
        "raw_s3_key": null,
    }))
    .into_response()
}

async fn h_ingest(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    record(&st, "/v1/ingest", &headers, body)
}

async fn h_ingest_raw(
    State(st): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    record(&st, "/v1/ingest/raw", &headers, body)
}

async fn spawn_ingest_server(script: Shared) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/ingest", post(h_ingest))
        .route("/v1/ingest/raw", post(h_ingest_raw))
        .with_state(script);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn sample_batch() -> IngestBatch {
    IngestBatch {
        batch_id: "batch_test".into(),
        device_id: "dev_test".into(),
        daemon_version: "daemon-0.0.0".into(),
        events: vec![],
        segments: vec![],
        tool_calls: vec![],
        session_installs: None,
        session_actors: None,
        session_titles: None,
        session_metadata: None,
        summarizer_mode: None,
        redactor_mode: None,
        repo_anchors: None,
        segment_generations: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_batch_commits_and_routes() {
    let script: Shared = Arc::new(Mutex::new(Recorder::default()));
    let base = spawn_ingest_server(script.clone()).await;

    let home = tempfile::tempdir().unwrap();
    std::env::set_var("MODELSTAT_HOME", home.path());
    std::env::set_var("DAEMON_API_URL", &base);
    std::env::remove_var("MODELSTAT_SUMMARIZER_MODE");

    // Seed a paired identity so upload takes the happy (bearer-present) path.
    save_identity(&DeviceIdentity {
        device_uuid: "uuid-test".into(),
        device_id: "dev_test".into(),
        bearer_token: "ds_live_testbearer".into(),
        claim_code: None,
        claim_url: None,
        hostname: "test-host".into(),
        created_at: "2026-07-16T00:00:00Z".into(),
        user_email: None,
        default_org_id: None,
    })
    .unwrap();

    let api = DeviceApi::new(Arc::new(Config::load("daemon-0.0.0")));
    let expected_mode = api.config().summarizer_mode();
    let batch = sample_batch();

    // ── 1. 2xx → Commit, receipt parsed, request well-formed ────────────────
    match api.upload_batch(&batch, false).await {
        UploadResult::Commit(rc) => {
            assert_eq!(rc.accepted, 3);
            assert_eq!(rc.new_sessions, 1);
            assert_eq!(rc.updated_sessions, 2);
            assert_eq!(rc.batch_id, "batch_srv");
        }
        UploadResult::Hold { reason, .. } => panic!("expected Commit, got Hold({reason})"),
    }
    {
        let s = script.lock().unwrap();
        assert_eq!(s.last_path, "/v1/ingest", "non-raw hits /v1/ingest");
        assert_eq!(
            s.last_auth.as_deref(),
            Some("Bearer ds_live_testbearer"),
            "bearer forwarded"
        );
        assert_eq!(
            s.last_body.as_ref().unwrap()["summarizer_mode"],
            json!(expected_mode),
            "summarizer_mode stamped on every batch"
        );
    }

    // ── 2. raw = true → /v1/ingest/raw ──────────────────────────────────────
    let _ = api.upload_batch(&batch, true).await;
    assert_eq!(
        script.lock().unwrap().last_path,
        "/v1/ingest/raw",
        "raw path routes to /v1/ingest/raw"
    );

    // ── 3. 429 reaches the gate: shrink now, grow back on sustained commits ──
    // Uploads are concurrent, so how many may be in flight has to come from the
    // server rather than from a constant we picked. This proves the signal is
    // actually plumbed: without it the limiter would sit at its start value
    // forever and a saturated edge would keep getting the same load.
    assert_eq!(
        api.upload_limit(),
        START_CONCURRENCY,
        "starts where it starts"
    );
    script.lock().unwrap().throttle_next = 1;
    // Still a Commit — a 429 is a retry, never a drop (feature §21.2). This is
    // the one place the test pays the ladder's first rung (~1s) in wall clock.
    assert!(
        api.upload_batch(&batch, false).await.is_commit(),
        "a throttled batch retries and commits, it does not drop"
    );
    assert_eq!(
        api.upload_limit(),
        MIN_CONCURRENCY,
        "the server's 429 halved the in-flight limit"
    );
    // …and it recovers, so one bad minute doesn't pin the daemon at sequential
    // uploads for the rest of its life. The retry above already banked one
    // commit, so this run of them crosses the threshold.
    for _ in 0..WINS_TO_GROW {
        assert!(api.upload_batch(&batch, false).await.is_commit());
    }
    assert!(
        api.upload_limit() > MIN_CONCURRENCY,
        "sustained commits earn the concurrency back"
    );

    std::env::remove_var("MODELSTAT_HOME");
    std::env::remove_var("DAEMON_API_URL");
}
