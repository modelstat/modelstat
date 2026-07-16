//! Authenticated device-API client — port of TS `apps/daemon/src/api.ts`
//! (feature §17.1). One client owns register, devices/me, recovery, the shared
//! device-request matrix, and heartbeat, so the auth + 401-recover + 5xx-backoff
//! behaviour is identical for every device call.
//!
//! Recovery is machine-stable re-register, NOT rotate-secret: a revoked/deleted
//! row has no valid current secret to rotate with, and re-registering with the
//! SAME deterministic `device_uuid` + `machine_id` lands on the same server row
//! (feature §4). `rotate_device_secret` is kept as API knowledge but unused.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::config::{Config, FreshIdentity};
use crate::ingest::{
    classify_status, describe_error, ingest_backoff, jitter, retry_after, IngestResponse,
    UploadDecision, UploadResult, INGEST_MAX_ATTEMPTS,
};
use crate::machine_key::{build_fingerprint, intended_device_uuid};
use modelstat_wire::{Fingerprint, IngestBatch, RegisterRequest};

/// Register-door response (`POST /v1/tokens`), the server's `{data:…}` payload.
#[derive(Debug, Clone, Deserialize)]
pub struct SelfRegisterResponse {
    pub device_id: String,
    pub device_uuid: String,
    /// Opaque `ds_live_<64hex>` bearer — stored verbatim, no format assumptions.
    pub device_secret: String,
    pub secret_prefix: String,
    #[serde(default)]
    pub claim_code: Option<String>,
    #[serde(default)]
    pub claim_url: Option<String>,
    pub status: String,
    #[serde(default)]
    pub user_id: Option<String>,
    /// Set when the server reused an existing row (same machine_id).
    #[serde(default)]
    pub re_registered: Option<bool>,
}

/// `GET /v1/devices/me` response (the fields the collector reads; unknown keys
/// are ignored).
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceMeResponse {
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub device_uuid: Option<String>,
    pub status: String,
    #[serde(default)]
    pub claim_code: Option<String>,
    #[serde(default)]
    pub claim_url: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

/// Heartbeat response (`POST /v1/devices/{id}/heartbeat`). `daemon_release` (the
/// self-update verdict) is passed through as opaque JSON — its typed consumption
/// lands with the daemon loop in M4.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HeartbeatResponse {
    #[serde(default)]
    pub daemon_release: Option<Value>,
    #[serde(default)]
    pub server_time: Option<String>,
    #[serde(default)]
    pub installations_upserted: Option<u64>,
    #[serde(default)]
    pub identities_upserted: Option<u64>,
}

/// `fetch_device_me` outcome. `Unauthorized` is the 401 sentinel that drives
/// recovery in the poll loop; everything else is a soft error to retry.
#[derive(Debug)]
pub enum DeviceMeError {
    Unauthorized,
    Other(String),
}

/// Module-level recover backoff (TS keeps it at module scope so it persists
/// across the per-second call sites; here it lives on the client instance).
struct RecoverState {
    backoff_until: Option<Instant>,
    attempt: usize,
}

/// Recover backoff schedule (feature §4): `[0, 2, 5, 15, 30, 60] s`.
const RECOVER_BACKOFF_MS: [u64; 6] = [0, 2_000, 5_000, 15_000, 30_000, 60_000];

/// Device-request backoff schedule (feature §17.1): `[1, 2.5, 5, 10, 20, 60] s`.
/// Port of the daemon-core `BACKOFF_MS` / `expBackoff`.
const DEVICE_BACKOFF_MS: [u64; 6] = [1_000, 2_500, 5_000, 10_000, 20_000, 60_000];

fn exp_backoff(attempt: usize) -> Duration {
    let i = attempt.min(DEVICE_BACKOFF_MS.len() - 1);
    Duration::from_millis(DEVICE_BACKOFF_MS[i])
}

/// One authed device-API client, holding the HTTP client, the shared [`Config`]
/// (identity cache + api-url resolution), and the recover backoff.
pub struct DeviceApi {
    http: Client,
    config: std::sync::Arc<Config>,
    recover: Mutex<RecoverState>,
}

impl Clone for DeviceApi {
    /// A near-free handle clone: the reqwest [`Client`] shares its connection
    /// pool + DNS cache across clones, and `config` is an `Arc`. The recover
    /// backoff starts FRESH — recovery is idempotent, self-rate-limited,
    /// machine-stable re-register, so each subsystem's handle can hold its own
    /// transient backoff without needing to share it. This is what lets the
    /// daemon hand a `&mut` handle to each `&mut self` trait consumer (the scan
    /// uploader, the SDK drain, the reconcile digest) while they run concurrently
    /// over ONE shared HTTP pool — DeviceApi's own methods are all `&self`, so a
    /// clone is purely to satisfy those traits' exclusive borrows.
    fn clone(&self) -> Self {
        DeviceApi {
            http: self.http.clone(),
            config: self.config.clone(),
            recover: Mutex::new(RecoverState {
                backoff_until: None,
                attempt: 0,
            }),
        }
    }
}

impl DeviceApi {
    pub fn new(config: std::sync::Arc<Config>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        DeviceApi {
            http,
            config,
            recover: Mutex::new(RecoverState {
                backoff_until: None,
                attempt: 0,
            }),
        }
    }

    pub fn config(&self) -> &std::sync::Arc<Config> {
        &self.config
    }

    /// Every JSON device response wraps its payload in `{ data: … }`; unwrap it
    /// tolerantly (a bare body — or a test stub — still parses). Port of the TS
    /// `unwrapData`.
    fn unwrap_data<T: DeserializeOwned>(body: Value) -> Result<T, String> {
        let inner = match &body {
            Value::Object(m) if m.contains_key("data") => body.get("data").cloned().unwrap(),
            _ => body,
        };
        serde_json::from_value(inner).map_err(|e| e.to_string())
    }

    /// Register against the ONE register door, `POST /v1/tokens`. Convergent on
    /// `machine_id`: a re-register of a known machine returns the same row with a
    /// fresh secret (`re_registered: true`). Port of TS `selfRegister`.
    pub async fn self_register(
        &self,
        device_uuid: String,
        fingerprint: Fingerprint,
    ) -> Result<SelfRegisterResponse, String> {
        let req = RegisterRequest {
            device_uuid,
            fingerprint,
            public_key: None,
        };
        let url = format!("{}/v1/tokens", self.config.api_url());
        let res = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("register request failed: {e}"))?;
        let code = res.status().as_u16();
        if code >= 300 {
            let body = res.text().await.unwrap_or_default();
            return Err(format!("register failed: {code} {body}"));
        }
        let body: Value = res
            .json()
            .await
            .map_err(|e| format!("register: bad JSON: {e}"))?;
        Self::unwrap_data(body)
    }

    /// `GET /v1/devices/me` — claim state. A `401` is the recovery sentinel; the
    /// body is discarded (callers only need the sentinel). Port of TS
    /// `fetchDeviceMe`.
    pub async fn fetch_device_me(&self, secret: &str) -> Result<DeviceMeResponse, DeviceMeError> {
        let url = format!("{}/v1/devices/me", self.config.api_url());
        let res = self
            .http
            .get(&url)
            .bearer_auth(secret)
            .send()
            .await
            .map_err(|e| DeviceMeError::Other(e.to_string()))?;
        let code = res.status().as_u16();
        if code == 401 {
            return Err(DeviceMeError::Unauthorized);
        }
        if code >= 300 {
            let body = res.text().await.unwrap_or_default();
            return Err(DeviceMeError::Other(format!(
                "devices/me failed: {code} {body}"
            )));
        }
        let body: Value = res
            .json()
            .await
            .map_err(|e| DeviceMeError::Other(e.to_string()))?;
        Self::unwrap_data(body).map_err(DeviceMeError::Other)
    }

    /// `POST /v1/devices/me/rotate-secret` — kept as API knowledge; deliberately
    /// NOT used by recovery (a revoked row can't rotate). Port of TS
    /// `rotateDeviceSecret`.
    pub async fn rotate_device_secret(&self, current_secret: &str) -> Result<Value, String> {
        let url = format!("{}/v1/devices/me/rotate-secret", self.config.api_url());
        let res = self
            .http
            .post(&url)
            .bearer_auth(current_secret)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let code = res.status().as_u16();
        if code >= 300 {
            let body = res.text().await.unwrap_or_default();
            return Err(format!("rotate-secret failed: {code} {body}"));
        }
        let body: Value = res.json().await.map_err(|e| e.to_string())?;
        Self::unwrap_data(body)
    }

    /// Machine-stable identity recovery: on 401/403 anywhere, drop the bearer and
    /// re-register with the SAME deterministic UUID + fingerprint, landing on the
    /// same server row. Self-rate-limited by the module-level backoff so a
    /// truly-deleted row can't hot-loop. Port of TS `recoverIdentity`.
    pub async fn recover_identity(&self) -> bool {
        {
            let r = self.recover.lock().unwrap();
            if let Some(until) = r.backoff_until {
                if Instant::now() < until {
                    return false; // still cooling down
                }
            }
        }
        // Drop the in-memory bearer first so re-register re-derives the
        // machine-stable UUID rather than reusing a stale cached value.
        self.config.set_bearer(None);
        let fp = build_fingerprint(self.config.version());
        match self.self_register(intended_device_uuid(), fp).await {
            Ok(res) => {
                let _ = self.config.save_fresh_identity(FreshIdentity {
                    device_uuid: res.device_uuid,
                    device_id: res.device_id,
                    bearer_token: res.device_secret,
                    claim_code: res.claim_code,
                    claim_url: res.claim_url,
                });
                let mut r = self.recover.lock().unwrap();
                r.attempt = 0;
                r.backoff_until = None;
                self.config.bearer().is_some()
            }
            Err(_) => {
                let mut r = self.recover.lock().unwrap();
                let delay = RECOVER_BACKOFF_MS[r.attempt.min(RECOVER_BACKOFF_MS.len() - 1)];
                r.attempt += 1;
                r.backoff_until = Some(Instant::now() + Duration::from_millis(delay));
                false
            }
        }
    }

    /// The shared authed request driver (feature §17.1). Returns the parsed JSON
    /// body on 2xx, or `None` for any non-recoverable outcome (no bearer,
    /// 400/422, SPA fallback, parse failure, attempts-exhausted). 401/403 →
    /// exactly one `recover_identity()` per attempt, then retry with the fresh
    /// bearer. 408/426/429/5xx + network → backoff + retry (HOLD, never drop).
    /// Port of TS `deviceRequest`.
    pub async fn device_request(
        &self,
        method: Method,
        url: &str,
        body: Option<&Value>,
        max_attempts: usize,
    ) -> Option<Value> {
        for attempt in 0..max_attempts {
            let Some(bearer) = self.config.bearer() else {
                // No token — one-shot recovery, then retry.
                if !self.recover_identity().await {
                    return None;
                }
                continue;
            };
            let mut req = self.http.request(method.clone(), url).bearer_auth(&bearer);
            if let Some(b) = body {
                req = req.json(b);
            }
            let res = match req.send().await {
                Ok(r) => r,
                Err(_) => {
                    // Network blip — back off + retry; never drop on a transient.
                    tokio::time::sleep(exp_backoff(attempt)).await;
                    continue;
                }
            };
            let code = res.status();
            if code.is_success() {
                if is_spa_fallback(&res) {
                    // 200 text/html = the SPA served a removed/undeployed route.
                    // Not transient (retrying won't help) - report loudly and
                    // return None without a parse crash (feature §17, §21.12).
                    eprintln!(
                        "[modelstat] device request to {} returned the dashboard (200 text/html) - route may be undeployed or misconfigured",
                        url
                    );
                    return None;
                }
                return res.json::<Value>().await.ok(); // parse failure → None
            }
            if code == StatusCode::UNAUTHORIZED || code == StatusCode::FORBIDDEN {
                if !self.recover_identity().await {
                    return None;
                }
                continue; // retry with the fresh bearer
            }
            if code == StatusCode::BAD_REQUEST || code == StatusCode::UNPROCESSABLE_ENTITY {
                // Permanent client error — will never succeed as-is (log ≤300).
                // Slice by CHARS, not bytes — a multibyte char straddling byte 300
                // would panic a byte-slice (matches the `.chars().take(..)` upload path).
                let text = res.text().await.unwrap_or_default();
                let snippet: String = text.chars().take(300).collect();
                eprintln!(
                    "[modelstat] device request rejected {} {}: {}",
                    url,
                    code.as_u16(),
                    snippet
                );
                return None;
            }
            let c = code.as_u16();
            if c == 408 || c == 426 || c == 429 || c >= 500 {
                tokio::time::sleep(exp_backoff(attempt)).await;
                continue; // transient — HOLD + retry
            }
            // Any other 4xx — treat as permanent.
            return None;
        }
        None // attempts exhausted
    }

    /// Defensive authed GET that degrades to `None` and recovers on 401/403 —
    /// the reconcile/backfill read path (feature §17.1). No envelope unwrap
    /// (backfill responses are top-level). Port of TS `authedJsonGet`.
    pub async fn authed_json_get<T: DeserializeOwned>(&self, url: &str) -> Option<T> {
        let raw = self.device_request(Method::GET, url, None, 3).await?;
        serde_json::from_value(raw).ok()
    }

    /// Authenticated heartbeat POST (feature §6.4). Routed through the shared
    /// matrix, so it gets the same 401→recover + 5xx-backoff handling. Returns
    /// the unwrapped `.data` on success, else `None`. Port of TS `postHeartbeat`.
    pub async fn post_heartbeat(&self, device_id: &str, body: &Value) -> Option<HeartbeatResponse> {
        // device_id is always `dev_<hex>` (server-minted, URL-safe), so it needs
        // no percent-encoding — inserted directly, matching encodeURIComponent's
        // identity on that character set.
        let url = format!(
            "{}/v1/devices/{}/heartbeat",
            self.config.api_url(),
            device_id
        );
        let raw = self
            .device_request(Method::POST, &url, Some(body), 3)
            .await?;
        Self::unwrap_data(raw).ok()
    }

    /// Ship one batch to `/v1/ingest` (or `/v1/ingest/raw` when `raw` — the cloud
    /// per-session path for turns the server summarises itself). The never-drop
    /// uploader (feature §17.1/§17.2, §21.2): a 2xx COMMITS; 401/403 reauths once
    /// (machine-stable recover) then retries; **everything else HOLDS + retries**
    /// with jittered backoff (`Retry-After` honored as a floor). A non-commit
    /// returns [`UploadResult::Hold`] — the scan loop leaves the file's cursor
    /// un-advanced and re-sends the SAME batch next cycle, so good data is never
    /// dropped. Port of TS `IngestClient.upload` + `apps/daemon/src/api.ts::uploadBatch`.
    pub async fn upload_batch(&self, batch: &IngestBatch, raw: bool) -> UploadResult {
        let path = if raw { "/v1/ingest/raw" } else { "/v1/ingest" };
        let url = format!("{}{}", self.config.api_url().trim_end_matches('/'), path);
        // Stamp the daemon's summarizer mode on every batch (both paths — the
        // server persists it as the scope's last-seen mode for ops alerts) and
        // clamp every bounded string to its schema byte cap ONCE, outside the
        // retry loop: the payload is identical across attempts, and the clamp
        // guarantees no permanently-rejectable (over-cap) batch. Rust strings are
        // inherently well-formed UTF-8, so TS's `wellFormedStringify` is a no-op.
        let mut wire = batch.clone();
        wire.summarizer_mode = Some(self.config.summarizer_mode());
        wire.clamp();

        let mut last_fetch_error: Option<String> = None;
        for attempt in 0..INGEST_MAX_ATTEMPTS {
            let Some(bearer) = self.config.bearer() else {
                // No token — one-shot recovery, then retry. If recovery fails or
                // is backing off, HOLD (never drop): retried next cycle.
                if !self.recover_identity().await {
                    return UploadResult::Hold("no_token".to_string());
                }
                continue;
            };
            let res = match self
                .http
                .post(&url)
                .bearer_auth(&bearer)
                .json(&wire)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    // Network blip — surface the real cause chain, back off + retry.
                    let detail = describe_error(&e);
                    eprintln!("[modelstat] ingest fetch failed (attempt {attempt}): {detail}");
                    last_fetch_error = Some(detail);
                    tokio::time::sleep(jitter(ingest_backoff(attempt))).await;
                    continue;
                }
            };
            let status = res.status().as_u16();
            match classify_status(status, attempt) {
                UploadDecision::Commit => {
                    // A 2xx with an opaque/empty body still reads as a commit.
                    let receipt = res.json::<IngestResponse>().await.unwrap_or_default();
                    return UploadResult::Commit(receipt);
                }
                UploadDecision::Reauth => {
                    // Bearer revoked/rotated server-side — recover by machine-stable
                    // re-register, then retry with the fresh bearer.
                    if !self.recover_identity().await {
                        return UploadResult::Hold("reauth_failed".to_string());
                    }
                    continue;
                }
                UploadDecision::Backoff(base) => {
                    // Read `Retry-After` (a FLOOR) BEFORE the body consumes `res`;
                    // add jitter so a fleet doesn't retry in lockstep.
                    let delay = jitter(base.max(retry_after(res.headers())));
                    // A 4xx body usually says WHY (a WAF/edge notice, a validation
                    // message) — surface ≤500 chars so a stuck batch is
                    // diagnosable. 5xx/429 bodies are noise; drop unread.
                    let body = if (400..500).contains(&status) {
                        res.text()
                            .await
                            .ok()
                            .map(|t| t.chars().take(500).collect::<String>())
                            .filter(|t| !t.is_empty())
                    } else {
                        None
                    };
                    eprintln!(
                        "[modelstat] ingest backoff status={status} delay_ms={} attempt={attempt}{}",
                        delay.as_millis(),
                        body.map(|b| format!(" body={b}")).unwrap_or_default()
                    );
                    tokio::time::sleep(delay).await;
                    continue; // HOLD + retry
                }
            }
        }
        // Attempts exhausted — HOLD (never drop). Surface the underlying network
        // cause when every attempt failed at the fetch layer, else a generic reason.
        let reason = last_fetch_error
            .map(|e| format!("network: {e}"))
            .unwrap_or_else(|| "max_attempts_exceeded".to_string());
        UploadResult::Hold(reason)
    }
}

/// The SPA fallback returns `200 text/html` for a removed route — detect it
/// BEFORE parsing so a route removal returns `null` (logged loudly by the
/// caller) instead of throwing
/// "Unexpected token <". Port of TS `isSpaFallback`.
fn is_spa_fallback(res: &reqwest::Response) -> bool {
    res.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase().contains("text/html"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_backoff_schedule() {
        assert_eq!(exp_backoff(0), Duration::from_millis(1_000));
        assert_eq!(exp_backoff(1), Duration::from_millis(2_500));
        assert_eq!(exp_backoff(5), Duration::from_millis(60_000));
        assert_eq!(exp_backoff(99), Duration::from_millis(60_000), "clamped");
    }

    #[test]
    fn recover_backoff_schedule_is_frozen() {
        assert_eq!(
            RECOVER_BACKOFF_MS,
            [0, 2_000, 5_000, 15_000, 30_000, 60_000]
        );
    }

    #[test]
    fn unwrap_data_handles_envelope_and_bare() {
        let enveloped = serde_json::json!({"data": {"status": "ok"}, "summary": "x"});
        let v: Value = DeviceApi::unwrap_data(enveloped).unwrap();
        assert_eq!(v["status"], "ok");
        let bare = serde_json::json!({"status": "ok"});
        let v: Value = DeviceApi::unwrap_data(bare).unwrap();
        assert_eq!(v["status"], "ok");
    }
}
