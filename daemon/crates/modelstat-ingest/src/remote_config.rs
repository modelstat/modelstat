//! Server-delivered config — the client half of `GET /v1/config/{kind}`
//! (feature §17.1), and the reason a calibration constant no longer needs a
//! release to change.
//!
//! **Trust is the TLS connection to the configured api origin, and nothing
//! else.** There is no payload signature and no request signing: every kind is
//! pure data whose only authority is over this daemon's own behaviour, the
//! origin is already the one we hand telemetry to, and a signing scheme would
//! add a key to distribute and rotate without removing a single attacker who can
//! already answer as that origin. What keeps a bad payload harmless is the
//! *shape of the kinds themselves* — the `policies` bundle can only ADD
//! redaction, calibration values are clamped to sane bounds — not a signature.
//! (The retired TS line specified Ed25519 here; it was dropped before it ever
//! shipped. Don't reintroduce it.)
//!
//! One pass over a kind:
//!
//! 1. `GET {api}/v1/config/{kind}` — no credentials; the route is public.
//! 2. Shape-validate with the kind's own validator. Anything unreadable is
//!    refused whole; what the *validator* tolerates is the kind's business.
//! 3. Version-gate: a payload must be strictly newer than what is held, so a
//!    stale cache, a botched rollback, or a lagging replica can never move this
//!    device backwards.
//! 4. Write through to `~/.modelstat/config/<kind>.json` (atomic, 0600) and swap
//!    it in for the next reader.
//!
//! Resolution order is therefore memory → disk → compiled-in fallback, and every
//! failure is a no-op: an offline daemon comes back on the last config it saw,
//! and a daemon that has never reached the server runs on the bundled default.
//!
//! The channel is deliberately vocabulary-free — it knows "JSON carrying a
//! monotonic `version`" and nothing about policies or calibration. A new kind is
//! a validator plus a call site, never an edit here.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::paths::{ensure_home, home_path, set_dir_0700};

/// How long one fetch may take end to end. Short on purpose: this is a few
/// hundred bytes of JSON, it runs on a timer, and a hung request would just
/// delay the next attempt by six hours.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Extreme size guard on a config body. Real payloads are kilobytes; this only
/// exists so a captive portal or a wedged proxy can't hand us something absurd
/// to buffer. A body over the cap is refused like any other unusable answer.
const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// A payload and the monotonic version it arrived with — the pair every kind
/// carries and the only thing the channel understands about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Versioned<T> {
    pub version: u64,
    pub value: T,
}

/// Shape-validate a payload for one kind. `None` refuses it — the channel then
/// keeps whatever it already held. A validator is free to be lenient INSIDE a
/// payload (skipping entries it can't use); returning `None` means the payload
/// as a whole is not this kind.
pub type Validate<T> = fn(&str) -> Option<Versioned<T>>;

/// The `config/` directory under the daemon home — one file per kind, next to
/// `identity.json` and `state.json`.
pub fn config_dir() -> PathBuf {
    home_path("config")
}

/// Where a kind's last good payload is cached.
pub fn config_cache_path(kind: &str) -> PathBuf {
    config_dir().join(format!("{kind}.json"))
}

/// Kinds are compiled-in slugs, but a kind names a file, so refuse anything that
/// could point the cache outside its directory. Total by construction: length
/// bounded, alphabet fixed, no separators and no dots.
fn safe_kind(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= 64
        && kind
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// One config kind, live for the life of the process: seeded from disk at
/// construction (instant, offline-safe), refreshed on a timer, and always
/// readable.
pub struct ConfigChannel<T> {
    kind: &'static str,
    validate: Validate<T>,
    /// The best payload held. `RwLock<Arc<_>>` so a reader pays a lock long
    /// enough to clone an `Arc` and a mid-run refresh reaches the next reader
    /// without disturbing the one in flight.
    held: RwLock<Arc<Versioned<T>>>,
    /// Set after a failure is logged, cleared by the next success — so a box
    /// that is offline for a week says so once, not once every six hours.
    warned: AtomicBool,
    http: reqwest::Client,
}

impl<T> ConfigChannel<T> {
    /// Build a channel for `kind`, seeded with `bundled` and then with the disk
    /// cache when it holds something at least as new. Does no network I/O.
    pub fn new(kind: &'static str, validate: Validate<T>, bundled: Versioned<T>) -> Self {
        debug_assert!(safe_kind(kind), "config kind must be a plain slug: {kind}");
        let seed = read_cache(kind, validate)
            .filter(|cached| cached.version >= bundled.version)
            .unwrap_or(bundled);
        ConfigChannel {
            kind,
            validate,
            held: RwLock::new(Arc::new(seed)),
            warned: AtomicBool::new(false),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(FETCH_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// The payload in force. Poison-safe: a panicked writer must not wedge every
    /// future read.
    pub fn current(&self) -> Arc<Versioned<T>> {
        self.held.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Fetch once and adopt the answer if it is strictly newer than what is
    /// held. Returns the new payload when it moved, `None` when nothing changed
    /// — for any reason at all, since every failure here means "keep what we
    /// have". Never errors: this runs on a best-effort timer.
    pub async fn refresh(&self, api_url: &str) -> Option<Arc<Versioned<T>>> {
        let fetched = self.fetch(api_url).await?;
        let current_version = self.current().version;
        if fetched.parsed.version <= current_version {
            // Not an error: the server is simply serving what we already have.
            // A LOWER version is the anti-rollback case — say so, because it
            // means someone published a regression.
            if fetched.parsed.version < current_version {
                modelstat_log::log_warn!(
                    "config {} served version {} but this device already holds {} — \
                     ignoring it; config versions only ever move forward",
                    self.kind,
                    fetched.parsed.version,
                    current_version
                );
            }
            return None;
        }
        write_cache(self.kind, &fetched.raw);
        let adopted = Arc::new(fetched.parsed);
        *self.held.write().unwrap_or_else(|e| e.into_inner()) = adopted.clone();
        modelstat_log::log_info!(
            "config {} updated to version {} (was {current_version})",
            self.kind,
            adopted.version
        );
        Some(adopted)
    }

    /// GET + validate. `None` on anything that isn't a usable payload; the
    /// reason is logged once per failure run.
    async fn fetch(&self, api_url: &str) -> Option<Fetched<T>> {
        if !safe_kind(self.kind) {
            return None;
        }
        let url = format!("{}/v1/config/{}", api_url.trim_end_matches('/'), self.kind);
        let resp = match self.http.get(&url).send().await {
            Ok(r) => r,
            Err(e) => return self.hold(format!("unreachable: {e}")),
        };
        let status = resp.status().as_u16();
        if status == 404 {
            // The server doesn't serve this kind (yet). Expected while a kind is
            // rolling out: the compiled-in default is the answer, not an error.
            return self.hold(format!(
                "not served by {api_url} (HTTP 404) — using the built-in default"
            ));
        }
        if !resp.status().is_success() {
            return self.hold(format!("HTTP {status}"));
        }
        let body = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return self.hold(format!("body unreadable: {e}")),
        };
        if body.len() > MAX_CONFIG_BYTES {
            return self.hold(format!("payload is {} bytes — refusing it", body.len()));
        }
        let raw = match String::from_utf8(body.to_vec()) {
            Ok(s) => s,
            Err(_) => return self.hold("payload is not UTF-8".to_string()),
        };
        let Some(parsed) = (self.validate)(&raw) else {
            return self.hold("payload failed shape validation".to_string());
        };
        self.warned.store(false, Ordering::Relaxed);
        Some(Fetched { raw, parsed })
    }

    /// Log a failure the first time it happens (until the next success) and hold
    /// what we have.
    fn hold(&self, reason: String) -> Option<Fetched<T>> {
        if !self.warned.swap(true, Ordering::Relaxed) {
            modelstat_log::log_warn!(
                "config {} not refreshed — {reason}; continuing on version {}",
                self.kind,
                self.current().version
            );
        }
        None
    }
}

/// A validated payload plus the exact bytes it came in, so the cache stores what
/// the server served rather than a re-serialization of it.
struct Fetched<T> {
    raw: String,
    parsed: Versioned<T>,
}

/// Read a kind's cache and RE-VALIDATE it. Bytes on disk are never trusted for
/// having been written by us once: a torn, hand-edited, or truncated file is
/// refused exactly like a bad network answer, and the caller falls through to
/// the compiled-in default.
fn read_cache<T>(kind: &str, validate: Validate<T>) -> Option<Versioned<T>> {
    if !safe_kind(kind) {
        return None;
    }
    let raw = std::fs::read_to_string(config_cache_path(kind)).ok()?;
    match validate(&raw) {
        Some(v) => Some(v),
        None => {
            modelstat_log::log_warn!(
                "config {kind} cache is unusable — ignoring it and using the built-in default"
            );
            None
        }
    }
}

/// Write a kind's payload atomically (`<file>.<pid>.tmp` + rename, 0600), the
/// same shape `identity.json` and `state.json` use. Best-effort: a cache we
/// couldn't write only costs the next boot a fetch.
fn write_cache(kind: &str, raw: &str) {
    if !safe_kind(kind) {
        return;
    }
    if ensure_home().is_err() {
        return;
    }
    let dir = config_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    set_dir_0700(&dir);
    let path = config_cache_path(kind);
    let tmp = crate::atomic::with_pid_tmp(&path);
    if std::fs::write(&tmp, raw).is_err() {
        return;
    }
    crate::atomic::set_file_0600(&tmp);
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    crate::atomic::set_file_0600(&path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env_lock;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// The stand-in kind: a bare number, so these tests exercise the channel and
    /// not somebody else's schema.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Knob(u64);

    /// `{"version":N,"knob":M}` — anything else is not this kind.
    fn validate_knob(raw: &str) -> Option<Versioned<Knob>> {
        let v: serde_json::Value = serde_json::from_str(raw).ok()?;
        let version = v.get("version")?.as_u64()?;
        let knob = v.get("knob")?.as_u64()?;
        Some(Versioned {
            version,
            value: Knob(knob),
        })
    }

    fn bundled() -> Versioned<Knob> {
        Versioned {
            version: 0,
            value: Knob(7),
        }
    }

    fn channel() -> ConfigChannel<Knob> {
        ConfigChannel::new("knob", validate_knob, bundled())
    }

    /// A one-shot HTTP/1.1 server that answers every request from a canned list,
    /// in order, then stops. `Connection: close` keeps the framing trivial.
    fn serve(responses: Vec<(u16, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        addr
    }

    fn with_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _g = test_env_lock();
        std::env::set_var("MODELSTAT_HOME", dir);
        let out = f();
        std::env::remove_var("MODELSTAT_HOME");
        out
    }

    #[test]
    fn a_fresh_channel_starts_on_the_bundled_default() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let c = channel();
            assert_eq!(*c.current(), bundled());
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_newer_payload_is_adopted_and_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = serve(vec![(200, r#"{"version":4,"knob":42}"#.into())]);
        let cached = with_home(tmp.path(), || {
            let c = channel();
            let adopted = futures_block_on(c.refresh(&addr)).expect("adopted");
            assert_eq!(adopted.value, Knob(42));
            assert_eq!(c.current().version, 4);
            std::fs::read_to_string(config_cache_path("knob")).expect("cache written")
        });
        assert_eq!(cached, r#"{"version":4,"knob":42}"#);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn versions_only_move_forward() {
        let tmp = tempfile::tempdir().unwrap();
        // 5 lands; then an older 4 and a repeat of 5 are both refused.
        let addr = serve(vec![
            (200, r#"{"version":5,"knob":1}"#.into()),
            (200, r#"{"version":4,"knob":2}"#.into()),
            (200, r#"{"version":5,"knob":3}"#.into()),
            (200, r#"{"version":6,"knob":4}"#.into()),
        ]);
        with_home(tmp.path(), || {
            let c = channel();
            assert!(futures_block_on(c.refresh(&addr)).is_some());
            assert_eq!(c.current().value, Knob(1));
            assert!(
                futures_block_on(c.refresh(&addr)).is_none(),
                "older refused"
            );
            assert_eq!(c.current().value, Knob(1));
            assert!(
                futures_block_on(c.refresh(&addr)).is_none(),
                "equal refused"
            );
            assert_eq!(c.current().value, Knob(1));
            assert!(
                futures_block_on(c.refresh(&addr)).is_some(),
                "newer adopted"
            );
            assert_eq!(c.current().value, Knob(4));
            // The rollback never reached the cache either.
            let cached = std::fs::read_to_string(config_cache_path("knob")).unwrap();
            assert_eq!(cached, r#"{"version":6,"knob":4}"#);
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_disk_cache_survives_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = serve(vec![(200, r#"{"version":9,"knob":11}"#.into())]);
        with_home(tmp.path(), || {
            futures_block_on(channel().refresh(&addr));
            // A new process, no network: the last good payload is still in force.
            let restarted = channel();
            assert_eq!(restarted.current().version, 9);
            assert_eq!(restarted.current().value, Knob(11));
        });
    }

    #[test]
    fn a_corrupt_cache_falls_back_to_bundled() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            std::fs::create_dir_all(config_dir()).unwrap();
            for junk in ["", "{", "not json at all", r#"{"version":3}"#] {
                std::fs::write(config_cache_path("knob"), junk).unwrap();
                assert_eq!(*channel().current(), bundled(), "junk: {junk:?}");
            }
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn garbage_and_errors_leave_the_held_value_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = serve(vec![
            (200, r#"{"version":2,"knob":22}"#.into()),
            (200, "<html>captive portal</html>".into()),
            (200, r#"{"version":"three","knob":1}"#.into()),
            (500, r#"{"error":"boom"}"#.into()),
            (404, r#"{"error":"unknown_config_kind"}"#.into()),
        ]);
        with_home(tmp.path(), || {
            let c = channel();
            assert!(futures_block_on(c.refresh(&addr)).is_some());
            for _ in 0..4 {
                assert!(futures_block_on(c.refresh(&addr)).is_none());
                assert_eq!(c.current().value, Knob(22), "held value is untouched");
            }
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreachable_server_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let c = channel();
            // Port 1 on loopback: nothing is listening, connect fails fast.
            assert!(futures_block_on(c.refresh("http://127.0.0.1:1")).is_none());
            assert_eq!(*c.current(), bundled());
        });
    }

    #[test]
    fn kind_names_that_could_escape_the_cache_dir_are_refused() {
        assert!(safe_kind("policies"));
        assert!(safe_kind("calibration"));
        assert!(!safe_kind(""));
        assert!(!safe_kind("../etc/passwd"));
        assert!(!safe_kind("a/b"));
        assert!(!safe_kind("a.b"));
        assert!(!safe_kind("Policies"));
        assert!(!safe_kind(&"x".repeat(65)));
    }

    /// `with_home` mutates process env, so it can't be held across an await;
    /// these tests drive the async refresh from inside it on the current runtime.
    fn futures_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}
