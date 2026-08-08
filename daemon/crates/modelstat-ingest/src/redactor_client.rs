//! The remote span classifier — [`PiiModel`] over HTTP (feature: redactor
//! modes). In `cloud` mode the endpoint is modelstat's `/v1/redact/*` behind
//! the device bearer; in `self-hosted` it is whatever the org runs at
//! `redactor_url` (same protocol, no auth unless they front it themselves).
//!
//! This client is the ONLY place floor-scrubbed-but-not-yet-spliced text
//! leaves the machine, so its failure posture is the redactor's: any answer it
//! cannot get — network down, server busy, protocol skew, a response that
//! doesn't line up with the request — is `None`, which the callers' fail-closed
//! machinery turns into a held flush. It never degrades, never ships around a
//! failure, and never logs the text it carries (counts and byte sizes only).
//!
//! `classify` is a synchronous trait method reached from async scan code, the
//! same shape as the on-device model's forward pass, and it bridges the same
//! way: `block_in_place` + `block_on` on the runtime the daemon already runs
//! (see `privacy_filter::label_window` for the precedent and the crash-loop
//! that taught it).

use std::time::Duration;

use modelstat_redact::remote::{
    ClassifyRequest, ClassifyResponse, MAX_REQUEST_BYTES, MAX_TEXTS_PER_REQUEST, REDACT_PROTOCOL,
};
use modelstat_redact::{PiiModel, PiiToken};

use crate::ingest::{jitter, retry_after};

/// Attempts per chunk before giving up (the flush holds and the scan retries
/// later — this bound is per-call patience, not durability).
const ATTEMPTS: usize = 3;
/// Fallback backoff when the server sent no `Retry-After`: 500ms · 2ⁿ, ≤ 8s.
fn backoff(attempt: usize) -> Duration {
    Duration::from_millis(500 * (1 << attempt.min(4))).min(Duration::from_secs(8))
}
/// Longest server-suggested wait we honor inline; anything larger means "come
/// back next flush", which is what `None` already does.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(30);

/// Soft byte budget per request body. Chunks aim under this; one oversize text
/// still ships alone (the hard cap [`MAX_REQUEST_BYTES`] is sized to fit it).
const CHUNK_BYTE_BUDGET: usize = 4 * 1024 * 1024;

/// One request's timeout — transfer plus a bounded inference wave server-side.
fn request_timeout() -> Duration {
    std::env::var("MODELSTAT_REDACTOR_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(60))
}

pub struct RemoteRedactor {
    /// Endpoint BASE (`https://modelstat.ai`, or the org's box). Paths are
    /// appended here, so cloud and self-hosted are one code path.
    base: String,
    /// `Authorization: Bearer …` when talking to modelstat (the device
    /// secret); `None` for a bare self-hosted box.
    bearer: Option<String>,
    http: reqwest::Client,
}

impl RemoteRedactor {
    pub fn new(base: impl Into<String>, bearer: Option<String>) -> Self {
        RemoteRedactor {
            base: base.into().trim_end_matches('/').to_string(),
            bearer,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(request_timeout())
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// [`Self::healthz`] from sync code (daemon boot, CLI probes) via the same
    /// bridge `classify` uses.
    pub fn healthz_blocking(&self) -> Option<modelstat_redact::remote::RedactHealth> {
        self.block_on(self.healthz())
    }

    /// `GET /v1/redact/healthz` — one attempt, no retry. `status` and the span
    /// cache's fingerprint read this; neither wants to wait out a backoff.
    pub async fn healthz(&self) -> Option<modelstat_redact::remote::RedactHealth> {
        let mut req = self.http.get(self.url("/v1/redact/healthz"));
        if let Some(b) = &self.bearer {
            req = req.bearer_auth(b);
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json().await.ok()
    }

    /// The blocking bridge for the sync [`PiiModel`] trait — the runtime the
    /// daemon runs when there is one, a throwaway one when there isn't (CLI
    /// probes, tests without a runtime).
    fn block_on<T: Send>(&self, fut: impl std::future::Future<Output = T> + Send) -> T {
        match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| h.block_on(fut))
            }
            // A current-thread runtime cannot block in place; hop to a fresh
            // thread with its own mini-runtime. Cold path (one-shot commands).
            Ok(_) | Err(_) => std::thread::scope(|s| {
                s.spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("mini runtime")
                        .block_on(fut)
                })
                .join()
                .expect("classify thread")
            }),
        }
    }

    /// POST one chunk, with retry on busy/unavailable/transport. `None` is
    /// "hold": the caller's flush machinery owns what happens next.
    async fn classify_chunk(&self, texts: &[String]) -> Option<Vec<Vec<PiiToken>>> {
        let body = ClassifyRequest {
            protocol: REDACT_PROTOCOL,
            texts: texts.to_vec(),
        };
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(jitter(backoff(attempt - 1))).await;
            }
            let mut req = self.http.post(self.url("/v1/redact/classify")).json(&body);
            if let Some(b) = &self.bearer {
                req = req.bearer_auth(b);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    modelstat_log::log_warn!(
                        "remote redactor unreachable (attempt {attempt}): {e}"
                    );
                    continue;
                }
            };
            let status = resp.status().as_u16();
            match status {
                200 => {
                    let parsed: ClassifyResponse = match resp.json().await {
                        Ok(p) => p,
                        Err(e) => {
                            modelstat_log::log_warn!("remote redactor sent unreadable JSON: {e}");
                            return None;
                        }
                    };
                    if parsed.protocol != REDACT_PROTOCOL {
                        modelstat_log::log_warn!(
                            "remote redactor speaks protocol {} (want {REDACT_PROTOCOL}) — \
                             holding; update the endpoint or the daemon",
                            parsed.protocol
                        );
                        return None;
                    }
                    if parsed.results.len() != texts.len() {
                        modelstat_log::log_warn!(
                            "remote redactor answered {} texts for {} sent — holding",
                            parsed.results.len(),
                            texts.len()
                        );
                        return None;
                    }
                    return Some(
                        parsed
                            .results
                            .into_iter()
                            .map(|toks| toks.into_iter().map(Into::into).collect())
                            .collect(),
                    );
                }
                // Busy / not ready: the server said when to come back.
                429 | 503 => {
                    let wait = retry_after(resp.headers()).min(RETRY_AFTER_CAP);
                    modelstat_log::log_warn!(
                        "remote redactor {} (attempt {attempt}) — retrying in {:?}",
                        if status == 429 { "busy" } else { "unavailable" },
                        wait
                    );
                    if !wait.is_zero() {
                        tokio::time::sleep(wait).await;
                    }
                }
                // Anything else (401, 400, 5xx) is not solved by retrying here.
                other => {
                    modelstat_log::log_warn!(
                        "remote redactor refused a chunk of {} texts: HTTP {other} — holding",
                        texts.len()
                    );
                    return None;
                }
            }
        }
        modelstat_log::log_warn!(
            "remote redactor did not answer after {ATTEMPTS} attempts — holding this flush"
        );
        None
    }

    /// Split `texts` into wire-sized chunks: at most [`MAX_TEXTS_PER_REQUEST`]
    /// texts and roughly [`CHUNK_BYTE_BUDGET`] bytes each; a single text over
    /// the budget still ships alone (the request cap fits it by construction).
    fn chunks(texts: &[String]) -> Vec<&[String]> {
        let mut out = Vec::new();
        let mut start = 0usize;
        let mut bytes = 0usize;
        for (i, t) in texts.iter().enumerate() {
            let over_count = i - start >= MAX_TEXTS_PER_REQUEST;
            let over_bytes = i > start && bytes + t.len() > CHUNK_BYTE_BUDGET;
            if over_count || over_bytes {
                out.push(&texts[start..i]);
                start = i;
                bytes = 0;
            }
            bytes += t.len();
        }
        if start < texts.len() {
            out.push(&texts[start..]);
        }
        out
    }
}

impl PiiModel for RemoteRedactor {
    fn classify(&self, text: &str) -> Option<Vec<PiiToken>> {
        self.classify_many(std::slice::from_ref(&text.to_string()))
            .map(|mut v| v.remove(0))
    }

    fn classify_many(&self, texts: &[String]) -> Option<Vec<Vec<PiiToken>>> {
        if texts.is_empty() {
            return Some(Vec::new());
        }
        debug_assert!(
            texts.iter().all(|t| t.len() < MAX_REQUEST_BYTES / 2),
            "a text exceeding the wire cap cannot be classified remotely"
        );
        self.block_on(async {
            let mut out = Vec::with_capacity(texts.len());
            for chunk in Self::chunks(texts) {
                out.extend(self.classify_chunk(chunk).await?);
            }
            Some(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A scripted HTTP/1.1 server: hands out the canned responses in order,
    /// records what it saw. `Connection: close` per response keeps the framing
    /// trivial — each attempt is one connection.
    struct Mock {
        addr: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    fn mock(responses: Vec<(u16, &'static str, String)>) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            for (status, extra_headers, body) in responses {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let mut buf = vec![0u8; 65536];
                let mut req = Vec::new();
                // Read headers + declared body (client always sends Content-Length).
                loop {
                    let n = sock.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(head_end) = find(&req, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&req[..head_end]).to_string();
                        let want: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if req.len() >= head_end + 4 + want {
                            break;
                        }
                    }
                }
                seen2
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&req).to_string());
                let reason = match status {
                    200 => "OK",
                    429 => "Too Many Requests",
                    503 => "Service Unavailable",
                    _ => "Error",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        Mock { addr, seen }
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn ok_body(results: &str) -> String {
        format!(r#"{{"protocol":1,"model":"privacy-filter@abc123def456","results":{results}}}"#)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_map_back_in_order_and_carry_offsets() {
        let m = mock(vec![(
            200,
            "",
            ok_body(
                r#"[[{"entity":"S-private_person","word":"Katherine","start":5,"end":14}],[]]"#,
            ),
        )]);
        let r = RemoteRedactor::new(&m.addr, Some("ds_live_test".into()));
        let out = r
            .classify_many(&["ping Katherine".into(), "nothing here".into()])
            .expect("healthy server answers");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][0].entity, "S-private_person");
        assert_eq!(out[0][0].start, Some(5));
        assert!(out[1].is_empty());
        let seen = m.seen.lock().unwrap();
        assert!(
            seen[0].contains("authorization: Bearer ds_live_test")
                || seen[0].contains("Authorization: Bearer ds_live_test"),
            "the device bearer must ride every request"
        );
        assert!(seen[0].starts_with("POST /v1/redact/classify"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_then_ok_retries_and_succeeds() {
        let m = mock(vec![
            (
                429,
                "Retry-After: 0\r\n",
                r#"{"error":"redactor_busy"}"#.into(),
            ),
            (200, "", ok_body("[[]]")),
        ]);
        let r = RemoteRedactor::new(&m.addr, None);
        let out = r.classify_many(&["hello".into()]);
        assert_eq!(out, Some(vec![vec![]]));
        assert_eq!(m.seen.lock().unwrap().len(), 2, "one retry, then success");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_that_stays_down_is_a_hold_not_a_degrade() {
        let m = mock(vec![
            (
                503,
                "Retry-After: 0\r\n",
                r#"{"error":"redactor_unavailable"}"#.into(),
            ),
            (
                503,
                "Retry-After: 0\r\n",
                r#"{"error":"redactor_unavailable"}"#.into(),
            ),
            (
                503,
                "Retry-After: 0\r\n",
                r#"{"error":"redactor_unavailable"}"#.into(),
            ),
        ]);
        let r = RemoteRedactor::new(&m.addr, None);
        assert_eq!(r.classify_many(&["x".into()]), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_misaligned_answer_is_refused() {
        // Two texts sent, one answer back: splicing would misattribute spans.
        let m = mock(vec![(200, "", ok_body("[[]]"))]);
        let r = RemoteRedactor::new(&m.addr, None);
        assert_eq!(r.classify_many(&["a".into(), "b".into()]), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn protocol_skew_is_refused() {
        let m = mock(vec![(
            200,
            "",
            r#"{"protocol":2,"model":"privacy-filter@abc","results":[[]]}"#.into(),
        )]);
        let r = RemoteRedactor::new(&m.addr, None);
        assert_eq!(r.classify_many(&["x".into()]), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unretryable_status_gives_up_immediately() {
        let m = mock(vec![(401, "", r#"{"error":"unauthorized"}"#.into())]);
        let r = RemoteRedactor::new(&m.addr, Some("ds_live_stale".into()));
        assert_eq!(r.classify_many(&["x".into()]), None);
        assert_eq!(m.seen.lock().unwrap().len(), 1, "401 is not retried");
    }

    #[test]
    fn chunking_respects_count_and_byte_budgets() {
        let many: Vec<String> = (0..70).map(|i| format!("t{i}")).collect();
        let chunks = RemoteRedactor::chunks(&many);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), MAX_TEXTS_PER_REQUEST);
        assert_eq!(chunks[1].len(), 6);

        let big = "x".repeat(CHUNK_BYTE_BUDGET);
        let texts = vec![big.clone(), "small".into(), big];
        let chunks = RemoteRedactor::chunks(&texts);
        assert_eq!(
            chunks.iter().map(|c| c.len()).collect::<Vec<_>>(),
            vec![1, 1, 1],
            "a budget-sized text ships alone, and never drags a neighbour over"
        );
        assert_eq!(chunks[1][0], "small");
    }

    #[test]
    fn empty_input_needs_no_server_at_all() {
        let r = RemoteRedactor::new("http://127.0.0.1:1", None);
        assert_eq!(r.classify_many(&[]), Some(Vec::new()));
    }
}
