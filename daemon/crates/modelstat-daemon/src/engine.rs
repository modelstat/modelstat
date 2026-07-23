//! Engine + on-device model wiring for the daemon-main loop — the concrete
//! `Summarizer` / `Embedder` / `NerModel` triple the scan + drain pipelines run
//! over, resolved from the install-time summarizer mode (feature §9.2, §9.5).
//!
//! Three moving parts:
//!   1. [`engine_base_url`] — where the summarizer protocol-v1 engine lives.
//!      `local` (and the unused-engine `cloud` default) reach the loopback engine
//!      at `http://127.0.0.1:<port>` (port from `summarizer.json`, default 4321);
//!      `self-hosted` is the same engine on the org's box (`selfHostedUrl`).
//!   2. [`build_embedder`] / [`build_ner`] — the BGE embedder + BERT-NER. Real
//!      candle models when built `--features candle` AND their weights are present
//!      in the shared cache (populated by `connect`, §9.5); otherwise the fail-safe
//!      [`NoEmbedder`] / [`UnavailableNer`]. A candle load failure degrades LOUDLY
//!      to the fail-safe pair — which keeps cloud mode correctly FAIL-CLOSED (NER
//!      unavailable ⇒ the flush holds, never floor-only egress).
//!
//! The engine binary itself (llama.cpp) is NEVER linked here — the collector only
//! ever speaks the HTTP protocol to it (plan D4). No `modelstat-llm` dependency.

use std::path::PathBuf;

use modelstat_ingest::{home_path, Config};
use modelstat_pipeline::{Embedder, NoEmbedder};
use modelstat_redact::{NerModel, NerToken, UnavailableNer};

/// The loopback engine's default port — must match `modelstat-llm`'s
/// `DEFAULT_PORT` (kept in sync by hand, since the collector can't link that
/// crate). A mismatch just means the collector points at the wrong port and the
/// resilient summarizer reports the engine unreachable + holds — loud, not silent.
pub const DEFAULT_ENGINE_PORT: u16 = 4321;

/// Just the `port` field of `~/.modelstat/summarizer.json` — a deliberately
/// minimal, llama-free view of the engine config (`modelstat-llm::EngineConfig`
/// is the full schema, but the collector must not depend on that crate). Serde
/// ignores every other field, so this stays compatible as the engine config grows.
#[derive(serde::Deserialize)]
struct EnginePortView {
    #[serde(default = "default_port")]
    port: u16,
}
fn default_port() -> u16 {
    DEFAULT_ENGINE_PORT
}

/// Read the loopback engine's port from `summarizer.json`, falling back to
/// [`DEFAULT_ENGINE_PORT`] when the file is absent (engine not set up yet) or
/// unreadable. Never errors — a bad port just means the engine looks unreachable.
pub fn engine_port() -> u16 {
    let path = home_path("summarizer.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<EnginePortView>(&text)
            .map(|v| v.port)
            .unwrap_or(DEFAULT_ENGINE_PORT),
        Err(_) => DEFAULT_ENGINE_PORT,
    }
}

/// The summarizer engine base URL for the active mode (feature §9.2):
///   - `self-hosted` → the org's engine URL (`selfHostedUrl`; env-overridable).
///   - `local` / `cloud` → the loopback engine `http://127.0.0.1:<port>`. (Cloud
///     never calls it — summarisation is server-side — but the client is still
///     constructed; a harmless loopback URL keeps the type uniform.)
pub fn engine_base_url(config: &Config) -> String {
    if config.summarizer_mode() == "self-hosted" {
        let url = config.self_hosted_url();
        if !url.trim().is_empty() {
            return url;
        }
        // Misconfigured self-hosted (no URL) → point at loopback so the resilient
        // client reports "unreachable" loudly and holds, rather than panicking on
        // an empty base URL. `modelstat mode self-hosted --url …` is the fix.
        eprintln!(
            "[modelstat] self-hosted mode has no engine URL — set one with \
             `modelstat mode self-hosted --url <URL>`; summaries will hold until then"
        );
    }
    format!("http://127.0.0.1:{}", engine_port())
}

/// The base on-device model dir (`MODELSTAT_MODELS_DIR` override, else
/// `~/.modelstat/models`) — one cache for `connect` + the daemon so the ~250 MB
/// NER + BGE weights download once and survive upgrades (§9.5). The `hf/<name>`
/// cache subdir is owned by `modelstat-download` (`HfModel::dir`); callers pass
/// this BASE so the downloader and the loaders below agree on `<base>/hf/<name>`
/// (passing `.../models/hf` here previously doubled it to `.../models/hf/hf/…`).
fn models_cache_dir() -> PathBuf {
    std::env::var_os("MODELSTAT_MODELS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_path("models"))
}

/// Best-effort pre-warm of the layer-2 NER redactor model into the shared cache
/// (§9.5). Returns whether the model dir is now present. `connect`/`mode` call it
/// so the first scan runs at full redaction quality instead of racing a lazy
/// download; a failure is never fatal (the daemon self-heals — §9.5).
pub async fn ensure_ner_model() -> bool {
    modelstat_download::ensure_hf_model(
        &modelstat_download::BERT_NER,
        &models_cache_dir(),
        &modelstat_download::TtyProgress::new("PII redactor"),
    )
    .await
    .is_ok()
}

/// Best-effort pre-warm of the embedding model (§9.5); embeddings are fail-open
/// (absent ⇒ time-gap segmentation), so this is purely a warm-up.
pub async fn ensure_embedder_model() -> bool {
    modelstat_download::ensure_hf_model(
        &modelstat_download::BGE_SMALL,
        &models_cache_dir(),
        &modelstat_download::TtyProgress::new("embedder"),
    )
    .await
    .is_ok()
}

/// Resolve one model's on-disk directory: an explicit `env_override` wins,
/// otherwise `<base>/hf/<subdir>` — the exact path `modelstat-download` writes to
/// (`HfModel::dir`), so the loader and the downloader match. `connect` (M6, §9.5)
/// populates these dirs; until then they're absent and the loaders below fall back
/// to the fail-safe models.
#[cfg_attr(not(feature = "candle"), allow(dead_code))]
fn model_dir(env_override: &str, subdir: &str) -> PathBuf {
    std::env::var_os(env_override)
        .map(PathBuf::from)
        .unwrap_or_else(|| models_cache_dir().join("hf").join(subdir))
}

/// The daemon's embedder — the real candle BGE model when available, else the
/// fail-open [`NoEmbedder`] (empty vectors → segmentation degrades to the
/// time-gap heuristic; §9.5). A single concrete type so the generic scan
/// pipeline monomorphises once regardless of build features.
pub enum DaemonEmbedder {
    /// No embeddings — segmentation uses the time/turn/content heuristic.
    None(NoEmbedder),
    /// The candle BGE-small-en-v1.5 model (384-dim), loaded from the cache.
    #[cfg(feature = "candle")]
    Candle(modelstat_pipeline::embed::CandleEmbedder),
}

impl Embedder for DaemonEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        match self {
            DaemonEmbedder::None(e) => e.embed(text),
            #[cfg(feature = "candle")]
            DaemonEmbedder::Candle(e) => e.embed(text),
        }
    }
}

/// Build the embedder for this process. Loads the candle BGE model from the
/// shared cache when built `--features candle` and its weights are present;
/// otherwise (or on a load failure) uses [`NoEmbedder`]. Embeddings are
/// best-effort, so an absent model is a quiet info line, not a hold.
pub fn build_embedder() -> DaemonEmbedder {
    #[cfg(feature = "candle")]
    {
        let dir = model_dir("MODELSTAT_EMBED_MODEL_DIR", "bge-small-en-v1.5");
        match modelstat_pipeline::embed::CandleEmbedder::load(&dir) {
            Ok(e) => {
                eprintln!("[modelstat] embedder: candle BGE-small (384-dim) loaded");
                return DaemonEmbedder::Candle(e);
            }
            Err(err) => {
                eprintln!(
                    "[modelstat] embedder: BGE model not loadable at {} ({err}) — \
                     segmentation uses the time-gap heuristic until `connect` downloads it",
                    dir.display()
                );
            }
        }
    }
    DaemonEmbedder::None(NoEmbedder)
}

/// The daemon's NER redactor (redaction layer 2) — the real candle BERT-NER model
/// when available, else the fail-closed [`UnavailableNer`]. Critical for privacy:
/// when unavailable, cloud/self-hosted flushes HOLD (fail-closed, §9.5) rather
/// than shipping floor-only-redacted content off the machine.
pub enum DaemonNer {
    /// NER inactive — only the deterministic regex floor (layer 1) applies. In
    /// cloud/self-hosted this makes the flush fail-closed (holds, no egress).
    Unavailable(UnavailableNer),
    /// The candle BERT-base-NER model, loaded from the cache.
    #[cfg(feature = "candle")]
    Candle(modelstat_redact::ner::CandleNer),
}

impl NerModel for DaemonNer {
    fn classify(&self, text: &str) -> Option<Vec<NerToken>> {
        match self {
            DaemonNer::Unavailable(n) => n.classify(text),
            #[cfg(feature = "candle")]
            DaemonNer::Candle(n) => n.classify(text),
        }
    }
}

/// Build the NER redactor for this process. Loads the candle BERT-NER model from
/// the shared cache when built `--features candle` and its weights are present;
/// otherwise (or on a load failure) uses [`UnavailableNer`] — which the redaction
/// floor still backstops, and which keeps cloud/self-hosted fail-closed.
pub fn build_ner() -> DaemonNer {
    #[cfg(feature = "candle")]
    {
        let dir = model_dir("MODELSTAT_NER_MODEL_DIR", "bert-base-NER");
        match modelstat_redact::ner::CandleNer::load(&dir) {
            Ok(n) => {
                eprintln!("[modelstat] NER (redaction layer 2): candle BERT-NER loaded");
                return DaemonNer::Candle(n);
            }
            Err(err) => {
                eprintln!(
                    "[modelstat] NER (redaction layer 2): model not loadable at {} ({err}) — \
                     redaction floor (layer 1) still applies; cloud/self-hosted flushes HOLD \
                     (fail-closed) until `connect` downloads it",
                    dir.display()
                );
            }
        }
    }
    DaemonNer::Unavailable(UnavailableNer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize the env-mutating tests in this module — `cargo test` runs them
    /// on multiple threads in one process, and they all read/write the same
    /// process-global env (`MODELSTAT_SUMMARIZER_MODE`, `MODELSTAT_HOME`, …).
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with_env<T>(pairs: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let _guard = env_lock();
        let saved: Vec<(String, Option<String>)> = pairs
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in pairs {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let out = f();
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
        out
    }

    #[test]
    fn self_hosted_mode_uses_the_org_url() {
        with_env(
            &[
                ("MODELSTAT_SUMMARIZER_MODE", Some("self-hosted")),
                (
                    "MODELSTAT_SUMMARIZER_URL",
                    Some("https://engine.acme.internal:4321"),
                ),
            ],
            || {
                let config = Config::load("daemon-test");
                assert_eq!(
                    engine_base_url(&config),
                    "https://engine.acme.internal:4321"
                );
            },
        );
    }

    #[test]
    fn local_mode_uses_loopback_with_the_default_port() {
        with_env(
            &[
                ("MODELSTAT_SUMMARIZER_MODE", Some("local")),
                // No MODELSTAT_HOME override → summarizer.json absent → default port.
                ("MODELSTAT_SUMMARIZER_URL", None),
            ],
            || {
                let config = Config::load("daemon-test");
                let url = engine_base_url(&config);
                assert!(
                    url == format!("http://127.0.0.1:{DEFAULT_ENGINE_PORT}"),
                    "got {url}"
                );
            },
        );
    }

    #[test]
    fn engine_port_reads_summarizer_json_or_defaults() {
        let dir = std::env::temp_dir().join(format!("modelstat-eng-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("summarizer.json"),
            br#"{"port":4399,"bind":"127.0.0.1"}"#,
        )
        .unwrap();
        with_env(&[("MODELSTAT_HOME", Some(dir.to_str().unwrap()))], || {
            assert_eq!(engine_port(), 4399);
        });
        // A missing file → default.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        with_env(&[("MODELSTAT_HOME", Some(empty.to_str().unwrap()))], || {
            assert_eq!(engine_port(), DEFAULT_ENGINE_PORT);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_build_uses_fail_safe_models() {
        // Without --features candle, the models are always the fail-safe pair.
        // (With the feature, they still fall back unless weights are present.)
        assert!(matches!(build_embedder(), DaemonEmbedder::None(_)) || cfg!(feature = "candle"));
        assert!(matches!(build_ner(), DaemonNer::Unavailable(_)) || cfg!(feature = "candle"));
    }
}
