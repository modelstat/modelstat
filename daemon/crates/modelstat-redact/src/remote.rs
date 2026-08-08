//! The remote-redactor wire protocol — the shapes a daemon in `cloud` /
//! `self-hosted` redactor mode exchanges with a span-classification endpoint
//! (modelstat's `/v1/redact/*`, or an org's own box running the same service).
//!
//! Types only, deliberately: this crate stays HTTP-free. The client lives in
//! `modelstat-ingest` (it owns the device's server credentials), the servers
//! live elsewhere entirely, and everyone meets at these structs.
//!
//! What crosses the wire in these shapes is FLOOR-SCRUBBED text — layer 1 has
//! already run on-device, so secrets, emails, key-shaped blobs and home paths
//! are gone before any request is built. The response carries the model's raw
//! non-`"O"` tokens; the requesting device does the splicing itself
//! ([`crate::redactor`]), so a remote answer flows through the exact code path a
//! local one does and produces byte-identical redaction.

use serde::{Deserialize, Serialize};

use crate::pii::PiiToken;

/// Version of this contract. A server answering a different protocol is a
/// skew the client treats as "cannot answer" (hold), never as best-effort.
pub const REDACT_PROTOCOL: u32 = 1;

/// Most texts one request may carry.
pub const MAX_TEXTS_PER_REQUEST: usize = 64;

/// Largest request body either side accepts. Sized so one maximal turn (the
/// wire's 262,144-char excerpt cap, ×4 for UTF-8, plus JSON escaping) always
/// fits alone — a text that no request could carry would otherwise hold its
/// flush forever.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// `POST /v1/redact/classify` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifyRequest {
    pub protocol: u32,
    pub texts: Vec<String>,
}

/// One classified token on the wire — [`PiiToken`] with its field meanings
/// pinned: `entity` is the raw BIOES tag exactly as the checkpoint emits it,
/// `start`/`end` are CHAR offsets (Unicode scalar values) into the
/// corresponding request text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireToken {
    pub entity: String,
    pub word: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<usize>,
}

impl From<WireToken> for PiiToken {
    fn from(t: WireToken) -> Self {
        PiiToken {
            entity: t.entity,
            word: t.word,
            start: t.start,
            end: t.end,
        }
    }
}

impl From<&PiiToken> for WireToken {
    fn from(t: &PiiToken) -> Self {
        WireToken {
            entity: t.entity.clone(),
            word: t.word.clone(),
            start: t.start,
            end: t.end,
        }
    }
}

/// `POST /v1/redact/classify` success response. `results` aligns 1:1 with the
/// request's `texts`; each inner list holds the model's non-`"O"` tokens in
/// order. (Dropping `"O"` server-side is exact: the span decoder skips them
/// without looking, so their absence changes nothing but the payload size.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifyResponse {
    pub protocol: u32,
    /// Version-bearing model identity (`privacy-filter@<12 hex>`), derived
    /// from the serving weights. Cache keys include it, so a server-side model
    /// swap can never satisfy a stale cache.
    pub model: String,
    pub results: Vec<Vec<WireToken>>,
}

/// `GET /v1/redact/healthz` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactHealth {
    pub ok: bool,
    pub protocol: u32,
    pub model: String,
    pub model_loaded: bool,
}

/// Error body for every non-2xx (`redactor_busy`, `redactor_unavailable`,
/// `too_many_texts`, `too_large`, `unauthorized`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactError {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shapes are a cross-repo contract (core's endpoint implements
    /// them from the spec, not from this crate), so the exact JSON is pinned
    /// here — a serde attribute change that would drift the wire fails this
    /// test first.
    #[test]
    fn classify_shapes_are_pinned() {
        let req = ClassifyRequest {
            protocol: REDACT_PROTOCOL,
            texts: vec!["ping Katherine".into()],
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"protocol":1,"texts":["ping Katherine"]}"#
        );

        let resp: ClassifyResponse = serde_json::from_str(
            r#"{"protocol":1,"model":"privacy-filter@abc123def456","results":[[{"entity":"S-private_person","word":"Katherine","start":5,"end":14}]]}"#,
        )
        .unwrap();
        assert_eq!(resp.results.len(), 1);
        let tok: PiiToken = resp.results[0][0].clone().into();
        assert_eq!(tok.entity, "S-private_person");
        assert_eq!(tok.start, Some(5));
        assert_eq!(tok.end, Some(14));
    }

    #[test]
    fn absent_offsets_read_as_none_and_stay_off_the_wire() {
        let tok: WireToken = serde_json::from_str(r#"{"entity":"B-secret","word":"x"}"#).unwrap();
        assert_eq!(tok.start, None);
        assert_eq!(
            serde_json::to_string(&tok).unwrap(),
            r#"{"entity":"B-secret","word":"x"}"#
        );
    }

    #[test]
    fn health_shape_is_pinned() {
        let h: RedactHealth = serde_json::from_str(
            r#"{"ok":true,"protocol":1,"model":"privacy-filter@abc123def456","model_loaded":true}"#,
        )
        .unwrap();
        assert!(h.ok && h.model_loaded);
        assert_eq!(h.protocol, REDACT_PROTOCOL);
    }

    /// One maximal excerpt must always fit a request alone, or a big turn
    /// could never be classified and its flush would hold forever.
    #[test]
    fn a_maximal_single_text_fits_the_request_cap() {
        // 262,144 chars of 4-byte scalars, JSON-escaped — the worst realistic case.
        let text = "\u{1F512}".repeat(262_144);
        let req = ClassifyRequest {
            protocol: REDACT_PROTOCOL,
            texts: vec![text],
        };
        assert!(serde_json::to_vec(&req).unwrap().len() <= MAX_REQUEST_BYTES);
    }
}
