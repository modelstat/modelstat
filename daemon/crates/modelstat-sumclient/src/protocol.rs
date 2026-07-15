//! Summarizer protocol v1 (feature §10.4) — the tiny, deliberately
//! non-OpenAI-compatible contract between the collector and the engine. Shared
//! by the client (this crate) and the engine's axum server
//! (`modelstat-summarizer`). Golden fixtures under
//! `modelstat-wire/tests/golden/summarizer/` pin the wire shapes.

use serde::{Deserialize, Serialize};

/// The protocol version this collector/engine speaks. Carried in `/healthz`; a
/// collector warns once on skew and surfaces it in `status`.
pub const PROTOCOL_VERSION: u32 = 1;

/// The pinned model id the engine reports.
pub const MODEL_ID: &str = "qwen3.5-4b-q4_k_m";

/// `GET /healthz` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub protocol: u32,
    pub version: String,
    pub model: String,
    pub model_loaded: bool,
    /// `"metal"` | `"cpu"`.
    pub backend: String,
}

/// `POST /v1/complete` request body. The collector owns every prompt (frozen
/// verbatim, §18); the engine is a generic completion server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompleteRequest {
    pub system: String,
    pub user: String,
    pub temperature: f64,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
}

/// `POST /v1/complete` success body — reasoning already stripped by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteResponse {
    pub text: String,
}

/// The engine's error body (503 while loading / 500 on inference failure). The
/// client never echoes it to the caller — status only (§9) — but the engine
/// emits it and tests assert on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineError {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden(name: &str) -> String {
        let path = format!(
            "{}/../modelstat-wire/tests/golden/summarizer/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    #[test]
    fn healthz_matches_golden() {
        let h: HealthResponse = serde_json::from_str(&golden("healthz.json")).unwrap();
        assert!(h.ok);
        assert_eq!(h.protocol, PROTOCOL_VERSION);
        assert_eq!(h.model, MODEL_ID);
        assert!(h.model_loaded);
        assert_eq!(h.backend, "metal");
    }

    #[test]
    fn complete_request_matches_golden() {
        let r: CompleteRequest = serde_json::from_str(&golden("complete_request.json")).unwrap();
        assert_eq!(r.temperature, 0.2);
        assert_eq!(r.max_tokens, 1024);
        assert_eq!(r.top_k, Some(3));
        assert!(r.system.starts_with("You are the modelstat session summarizer"));
    }

    #[test]
    fn complete_response_matches_golden() {
        let r: CompleteResponse = serde_json::from_str(&golden("complete_response.json")).unwrap();
        assert!(r.text.contains("ingest retry matrix"));
    }

    #[test]
    fn loading_503_error_shape() {
        let e: EngineError = serde_json::from_str(&golden("loading_503.json")).unwrap();
        assert_eq!(e.error, "model_loading");
    }

    #[test]
    fn top_k_omitted_when_none() {
        let r = CompleteRequest {
            system: "s".into(),
            user: "u".into(),
            temperature: 0.2,
            max_tokens: 1024,
            top_k: None,
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert!(v.get("top_k").is_none());
    }
}
