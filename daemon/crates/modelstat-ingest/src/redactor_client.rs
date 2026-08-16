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
///
/// Sized in MODEL TIME, not in bytes we could technically send. The classifier
/// runs ~20k chars/sec on an idle laptop and several times slower on a shared
/// server, so this is the knob that decides whether one request is seconds of
/// work or minutes of it. It used to be 4 MiB — over an hour of inference in a
/// single request on a loaded box — which meant a request could not finish
/// before ANY caller's patience ran out: the daemon gave up, retried, and the
/// server burned its whole capacity on passes nobody would ever read. 64 KiB is
/// ~3s of model time on a laptop and well under a minute on a slow server, so a
/// request lands inside every timeout on the path with room to spare.
const CHUNK_BYTE_BUDGET: usize = 64 * 1024;

/// One request's timeout — transfer plus a bounded inference wave server-side.
///
/// The OUTERMOST deadline on a chain that already reports its own failures:
/// the sidecar gives up on one pass at 180s and says `redactor_deadline`, and
/// the ingest edge gives up on the sidecar at 200s and says
/// `redactor_unavailable`. Both of those are worth strictly more than our
/// silence, so we outwait them — at 60s we cut the server off mid-answer and
/// learned nothing, over and over. A genuinely dead endpoint still fails fast
/// on `connect_timeout`; this only bounds a server that accepted and then went
/// quiet, which is the one case where waiting is the informative move.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(240);

fn request_timeout() -> Duration {
    std::env::var("MODELSTAT_REDACTOR_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT)
}

/// Why one classify request failed — the split that decides whether asking
/// again with less can help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkError {
    /// The endpoint itself failed (unreachable after retries, busy beyond
    /// patience, bad credentials, protocol skew): no re-slicing of the texts
    /// changes that, so the whole call holds.
    Endpoint,
    /// The server received this request and refused it — the one failure
    /// where the CONTENT is implicated, so a smaller batch may still pass.
    Refused,
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

    /// POST one chunk, with retry on busy/unavailable/transport. A failure
    /// names which side of it failed, because the two demand opposite moves:
    /// [`ChunkError::Endpoint`] (unreachable, unauthorized, busy beyond
    /// patience, protocol skew) — asking again with different texts learns
    /// nothing, stop; [`ChunkError::Refused`] (any other non-2xx: the server
    /// looked at THIS request and said no) — the content is implicated, so the
    /// caller bisects rather than letting one poison text hold the rest.
    async fn classify_chunk(&self, texts: &[String]) -> Result<Vec<Vec<PiiToken>>, ChunkError> {
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
                            return Err(ChunkError::Endpoint);
                        }
                    };
                    if parsed.protocol != REDACT_PROTOCOL {
                        modelstat_log::log_warn!(
                            "remote redactor speaks protocol {} (want {REDACT_PROTOCOL}) — \
                             holding; update the endpoint or the daemon",
                            parsed.protocol
                        );
                        return Err(ChunkError::Endpoint);
                    }
                    if parsed.results.len() != texts.len() {
                        modelstat_log::log_warn!(
                            "remote redactor answered {} texts for {} sent — holding",
                            parsed.results.len(),
                            texts.len()
                        );
                        return Err(ChunkError::Endpoint);
                    }
                    return Ok(parsed
                        .results
                        .into_iter()
                        .map(|toks| toks.into_iter().map(Into::into).collect())
                        .collect());
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
                // Credentials are chunk-independent: a request the server won't
                // let in won't let a half of it in either.
                401 | 403 => {
                    modelstat_log::log_warn!(
                        "remote redactor rejected the device credentials (HTTP {status}) — holding"
                    );
                    return Err(ChunkError::Endpoint);
                }
                // Anything else (400, 413, 5xx): the server saw THIS request
                // and refused it — retrying it whole is not the move, but its
                // halves may still be answerable.
                other => {
                    modelstat_log::log_warn!(
                        "remote redactor refused a chunk of {} texts: HTTP {other}",
                        texts.len()
                    );
                    return Err(ChunkError::Refused);
                }
            }
        }
        modelstat_log::log_warn!(
            "remote redactor did not answer after {ATTEMPTS} attempts — holding this flush"
        );
        Err(ChunkError::Endpoint)
    }

    /// Classify one wire-sized chunk, isolating failures per text: an answered
    /// chunk fills its texts' slots; a refused chunk is halved and each half
    /// retried (as its own smaller batch) until the unanswerable text stands
    /// alone, so it fails exactly as a single-text request always has — its
    /// slot `None`, its 63 batch-mates answered. `Err` means the ENDPOINT
    /// failed: this chunk's remaining slots are filled with `None` and the
    /// caller should stop asking.
    fn bisect<'a>(
        &'a self,
        texts: &'a [String],
        out: &'a mut Vec<Option<Vec<PiiToken>>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            match self.classify_chunk(texts).await {
                Ok(answers) => {
                    out.extend(answers.into_iter().map(Some));
                    Ok(())
                }
                Err(ChunkError::Endpoint) => {
                    out.extend(texts.iter().map(|_| None));
                    Err(())
                }
                Err(ChunkError::Refused) if texts.len() == 1 => {
                    modelstat_log::log_warn!(
                        "remote redactor cannot classify one text of {} bytes — \
                         holding it (isolated; the rest of its batch is unaffected)",
                        texts[0].len()
                    );
                    out.push(None);
                    Ok(())
                }
                Err(ChunkError::Refused) => {
                    let mid = texts.len() / 2;
                    self.bisect(&texts[..mid], out).await?;
                    self.bisect(&texts[mid..], out).await
                }
            }
        })
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

    fn classify_each(&self, texts: &[String]) -> Vec<Option<Vec<PiiToken>>> {
        if texts.is_empty() {
            return Vec::new();
        }
        debug_assert!(
            texts.iter().all(|t| t.len() < MAX_REQUEST_BYTES / 2),
            "a text exceeding the wire cap cannot be classified remotely"
        );
        self.block_on(async {
            let mut out: Vec<Option<Vec<PiiToken>>> = Vec::with_capacity(texts.len());
            let mut endpoint_down = false;
            for chunk in Self::chunks(texts) {
                let filled_before = out.len();
                if endpoint_down || self.bisect(chunk, &mut out).await.is_err() {
                    // A dead endpoint answers nothing else this call — every
                    // slot the bisection never reached reads "unanswered".
                    endpoint_down = true;
                    out.resize(filled_before + chunk.len(), None);
                }
            }
            debug_assert_eq!(out.len(), texts.len(), "one slot per text, always");
            out
        })
    }

    fn classify_many(&self, texts: &[String]) -> Option<Vec<Vec<PiiToken>>> {
        self.classify_each(texts).into_iter().collect()
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

    /// `results` for `n` texts none of which carried any entity.
    fn empties(n: usize) -> String {
        format!("[{}]", vec!["[]"; n].join(","))
    }

    /// The `texts` a recorded request carried.
    fn texts_sent(req: &str) -> Vec<String> {
        let body = &req[req.find("\r\n\r\n").unwrap() + 4..];
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_string())
            .collect()
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

    /// The reason batching exists: a flush of many texts must ride FEW requests
    /// — at most [`MAX_TEXTS_PER_REQUEST`] texts each — and every answer must
    /// land back on its own text, across the chunk boundary included.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn many_texts_ride_few_requests_and_answers_keep_their_order() {
        // Distinguishable answers at the chunk seam: the 64th text (last of
        // request 1) and the 65th (first of request 2).
        let last_of_first = r#"[{"entity":"S-secret","word":"m63"}]"#;
        let first_of_second = r#"[{"entity":"S-secret","word":"m64"}]"#;
        let m = mock(vec![
            (
                200,
                "",
                ok_body(&format!("[{},{last_of_first}]", ["[]"; 63].join(","))),
            ),
            (
                200,
                "",
                ok_body(&format!("[{first_of_second},{}]", ["[]"; 5].join(","))),
            ),
        ]);
        let r = RemoteRedactor::new(&m.addr, None);
        let texts: Vec<String> = (0..70).map(|i| format!("t{i}")).collect();
        let out = r.classify_many(&texts).expect("healthy server answers");
        assert_eq!(out.len(), 70, "one answer per text");
        assert_eq!(out[63][0].word, "m63");
        assert_eq!(out[64][0].word, "m64");
        assert!(out[0].is_empty() && out[69].is_empty());
        let seen = m.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "70 texts are 2 requests, not 70");
        assert_eq!(texts_sent(&seen[0]).len(), MAX_TEXTS_PER_REQUEST);
        assert_eq!(texts_sent(&seen[1]).len(), 6);
        assert_eq!(texts_sent(&seen[0])[63], "t63");
        assert_eq!(texts_sent(&seen[1])[0], "t64");
    }

    /// One unclassifiable text must not hold its batch-mates hostage: a refused
    /// batch is halved until the poison stands alone, the clean texts get their
    /// answers, and the poison fails exactly as a single-text request does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bisection_isolates_a_poison_text_and_answers_the_rest() {
        let refused = || (400, "", r#"{"error":"too_large"}"#.to_string());
        let m = mock(vec![
            refused(),                       // [t0 t1 t2 t3] — the whole batch
            refused(),                       // [t0 t1] — poison's half
            (200, "", ok_body("[[]]")),      // [t0]
            refused(),                       // [t1] — the poison, isolated
            (200, "", ok_body(&empties(2))), // [t2 t3] — clean half, one request
        ]);
        let r = RemoteRedactor::new(&m.addr, None);
        let texts: Vec<String> = (0..4).map(|i| format!("t{i}")).collect();
        let each = r.classify_each(&texts);
        assert_eq!(
            each,
            vec![Some(vec![]), None, Some(vec![]), Some(vec![])],
            "63 hostages freed, one poison held"
        );
        // The all-or-nothing flush view still holds (no degrade, no egress).
        assert_eq!(each.into_iter().collect::<Option<Vec<_>>>(), None);
        let seen = m.seen.lock().unwrap();
        let sequence: Vec<Vec<String>> = seen.iter().map(|s| texts_sent(s)).collect();
        assert_eq!(
            sequence,
            vec![
                vec!["t0".to_string(), "t1".into(), "t2".into(), "t3".into()],
                vec!["t0".to_string(), "t1".into()],
                vec!["t0".to_string()],
                vec!["t1".to_string()],
                vec!["t2".to_string(), "t3".into()],
            ],
            "halve, isolate, ship the clean halves — nothing lost, nothing reordered"
        );
    }

    /// Auth is not a property of the texts: a 401 must fail the call in ONE
    /// request, never spend a bisection tree re-asking with the same dead key.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_credentials_hold_everything_without_bisecting() {
        let m = mock(vec![(401, "", r#"{"error":"unauthorized"}"#.into())]);
        let r = RemoteRedactor::new(&m.addr, Some("ds_live_stale".into()));
        let each = r.classify_each(&["a".into(), "b".into()]);
        assert_eq!(each, vec![None, None]);
        assert_eq!(m.seen.lock().unwrap().len(), 1, "401 is never bisected");
    }

    /// An endpoint that dies mid-call: the chunks already answered keep their
    /// answers (the span cache upstream will remember them), the rest hold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_endpoint_failure_mid_call_keeps_the_answers_already_won() {
        let unavailable = || {
            (
                503,
                "Retry-After: 0\r\n",
                r#"{"error":"redactor_unavailable"}"#.to_string(),
            )
        };
        let m = mock(vec![
            (200, "", ok_body(&empties(64))),
            unavailable(),
            unavailable(),
            unavailable(),
        ]);
        let r = RemoteRedactor::new(&m.addr, None);
        let texts: Vec<String> = (0..70).map(|i| format!("t{i}")).collect();
        let each = r.classify_each(&texts);
        assert_eq!(each.len(), 70);
        assert!(each[..64].iter().all(Option::is_some));
        assert!(each[64..].iter().all(Option::is_none));
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

    /// The deadlines on this path must NEST, innermost first: the sidecar gives
    /// up on one pass at 180s (`redactor_deadline`), the ingest edge gives up on
    /// the sidecar at 200s (`redactor_unavailable`), and only then may we give
    /// up. Both of those answers name a reason; ours names nothing. Cutting them
    /// off first is what made cloud redaction fail silently for days, so this
    /// pins our end of an ordering whose other end lives in another repo.
    #[test]
    fn we_outwait_the_deadlines_that_can_still_explain_themselves() {
        const SIDECAR_CLASSIFY_BUDGET: Duration = Duration::from_secs(180);
        const INGEST_EDGE_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(200);
        assert!(
            SIDECAR_CLASSIFY_BUDGET < INGEST_EDGE_UPSTREAM_TIMEOUT,
            "the sidecar must answer before the edge stops listening"
        );
        assert!(
            DEFAULT_REQUEST_TIMEOUT > INGEST_EDGE_UPSTREAM_TIMEOUT,
            "we must outwait the edge, or we trade its stated reason for our silence"
        );
    }

    /// One request is a slice of MODEL time, and the model runs ~20k chars/sec
    /// on good hardware and slower on a shared box. A budget that cannot finish
    /// inside the sidecar's own 180s pass budget guarantees every request is
    /// abandoned work — the shape of the original wedge.
    #[test]
    fn a_full_chunk_is_minutes_of_work_at_worst_not_hours() {
        const PESSIMISTIC_CHARS_PER_SEC: usize = 2_000;
        let worst_case_secs = CHUNK_BYTE_BUDGET / PESSIMISTIC_CHARS_PER_SEC;
        assert!(
            worst_case_secs < 180,
            "a full chunk needs ~{worst_case_secs}s on a slow box, which the \
             sidecar's 180s pass budget cannot absorb"
        );
    }

    #[test]
    fn empty_input_needs_no_server_at_all() {
        let r = RemoteRedactor::new("http://127.0.0.1:1", None);
        assert_eq!(r.classify_many(&[]), Some(Vec::new()));
    }
}
