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
        modelstat_log::log_error!(
            "self-hosted mode has no engine URL — set one with \
             `modelstat mode self-hosted --url <URL>`; summaries will hold until then"
        );
    }
    format!("http://127.0.0.1:{}", engine_port())
}

/// The base on-device model dir (`MODELSTAT_MODELS_DIR` override, else
/// `~/.modelstat/models`) — one cache for `connect` + the daemon so the ~560 MB
/// NER + BGE weights download once and survive upgrades (§9.5). The `hf/<name>`
/// cache subdir is owned by `modelstat-download` (`HfModel::dir`); callers pass
/// this BASE so the downloader and the loaders below agree on `<base>/hf/<name>`
/// (passing `.../models/hf` here previously doubled it to `.../models/hf/hf/…`).
pub fn models_cache_dir() -> PathBuf {
    std::env::var_os("MODELSTAT_MODELS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_path("models"))
}

/// Which of the daemon's model handles a downloaded model feeds.
///
/// An enum, not a string, on purpose: the self-heal matches on it exhaustively,
/// so adding a model to [`ON_DEVICE_MODELS`] without teaching the daemon to load
/// it is a COMPILE error rather than a silent no-op. That is the same shape of
/// mistake that left BGE un-downloaded for the whole life of the Rust daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSlot {
    Ner,
    Embedder,
}

/// One on-device model the collector needs.
pub struct OnDeviceModel {
    pub model: &'static modelstat_download::HfModel,
    /// Stable identifier — the `status --json` key. Never derive this from
    /// [`Self::label`]: that is prose, and prose gets reworded.
    pub key: &'static str,
    /// What a human calls it, in logs and `status`.
    pub label: &'static str,
    /// The handle it loads into once present.
    pub slot: ModelSlot,
}

/// Every on-device model, in ONE list.
///
/// The single list is the point: a model can no longer be added to the loader and
/// forgotten in the downloader, which is exactly how the BGE weights went
/// un-downloaded on every install — `ensure_embedder_model` existed and nothing
/// ever called it. Anything that downloads, heals, or reports models walks this.
pub const ON_DEVICE_MODELS: [OnDeviceModel; 2] = [
    OnDeviceModel {
        model: &modelstat_download::BERT_NER,
        key: "redactor",
        label: "PII redactor",
        slot: ModelSlot::Ner,
    },
    OnDeviceModel {
        model: &modelstat_download::BGE_SMALL,
        key: "embedder",
        label: "embedder",
        slot: ModelSlot::Embedder,
    },
];

/// The models whose files are NOT all on disk. Cheap (a `stat` per file),
/// side-effect-free — what `status` reports and what the self-heal acts on.
pub fn missing_models() -> Vec<&'static OnDeviceModel> {
    let dir = models_cache_dir();
    ON_DEVICE_MODELS
        .iter()
        .filter(|m| !m.model.is_present(&dir))
        .collect()
}

/// Pre-warm the layer-2 NER redactor into the shared cache (§9.5). Returns
/// whether it is now present. `connect`/`mode` call it so the first scan runs at
/// full redaction quality; the bounded [`RetryPolicy::interactive`] keeps a human
/// from waiting forever, and the daemon's self-heal finishes what this can't.
pub async fn ensure_ner_model() -> bool {
    ensure_model(&modelstat_download::BERT_NER, "PII redactor").await
}

/// Pre-warm the BGE embedder (§9.5) — segmentation's topic-shift boundary.
pub async fn ensure_embedder_model() -> bool {
    ensure_model(&modelstat_download::BGE_SMALL, "embedder").await
}

async fn ensure_model(model: &modelstat_download::HfModel, label: &str) -> bool {
    modelstat_download::ensure_hf_model(
        model,
        &models_cache_dir(),
        &modelstat_download::TtyProgress::new(label),
        &modelstat_download::RetryPolicy::interactive(),
    )
    .await
    .is_ok()
}

/// Download whatever [`missing_models`] reports, retrying until it lands.
///
/// The daemon spawns this in the background at boot. It exists because `connect`
/// is the ONLY other downloader: before this, a model that failed its one
/// download attempt — a network blip, a laptop lid closed mid-install — stayed
/// missing forever, and the user had no way to know. With cloud/self-hosted the
/// NER model is worse than a quality loss: flushes fail closed and HOLD, so the
/// daemon uploads nothing at all until it lands.
///
/// Returns the models it successfully fetched, so the caller can hot-swap them in
/// without a restart. Uses [`RetryPolicy::forever`] — nobody is waiting, and an
/// offline machine simply resumes when the network returns.
pub async fn heal_missing_models() -> Vec<&'static OnDeviceModel> {
    let missing = missing_models();
    if missing.is_empty() {
        return Vec::new();
    }
    let names: Vec<&str> = missing.iter().map(|m| m.label).collect();
    modelstat_log::log_warn!(
        "on-device models missing ({}) — downloading now; segmentation and \
         cloud/self-hosted redaction run degraded or held until they land",
        names.join(", ")
    );
    let dir = models_cache_dir();
    let mut healed = Vec::new();
    for entry in missing {
        // SilentSink: a per-chunk progress meter belongs to an interactive
        // `connect`, not to a log file that would get thousands of lines.
        match modelstat_download::ensure_hf_model(
            entry.model,
            &dir,
            &modelstat_download::SilentSink,
            &modelstat_download::RetryPolicy::forever(),
        )
        .await
        {
            Ok(path) => {
                modelstat_log::log_info!("{} model downloaded → {}", entry.label, path.display());
                healed.push(entry);
            }
            // `forever` only returns Err on a PERMANENT failure (bad URL, bad
            // checksum) — a real bug, not a blip, and it needs a human.
            Err(e) => modelstat_log::log_error!(
                "{} model could not be downloaded: {e} — it will stay missing \
                 until this is fixed; `modelstat status` shows the current state",
                entry.label
            ),
        }
    }
    healed
}

/// A model handle the daemon can replace while it runs.
///
/// The self-heal downloads a missing model minutes or hours into the process's
/// life, and the alternative to swapping is running degraded until the next
/// restart — which for a supervised daemon can be days. The lock is held only
/// long enough to clone an `Arc` (never across an await), so readers on the scan
/// path pay a nanosecond and the swap is invisible to them.
pub struct Swappable<T>(std::sync::RwLock<std::sync::Arc<T>>);

impl<T> Swappable<T> {
    pub fn new(value: T) -> Self {
        Self(std::sync::RwLock::new(std::sync::Arc::new(value)))
    }

    /// The current handle. Poison-safe: a panicked writer must not wedge every
    /// future scan.
    pub fn get(&self) -> std::sync::Arc<T> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Install a new handle. In-flight readers keep their old `Arc` and finish
    /// against it; the next reader sees the new one.
    pub fn set(&self, value: T) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = std::sync::Arc::new(value);
    }
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
/// best-effort — segmentation keeps its four size/time boundaries and the server
/// recomputes the abstract embedding it needs — so an absent model is a warning,
/// not a hold. It should never happen: `connect`/`mode` download it.
pub fn build_embedder() -> DaemonEmbedder {
    #[cfg(feature = "candle")]
    {
        let dir = model_dir("MODELSTAT_EMBED_MODEL_DIR", "bge-small-en-v1.5");
        match modelstat_pipeline::embed::CandleEmbedder::load(&dir) {
            Ok(e) => {
                modelstat_log::log_info!("embedder: candle BGE-small (384-dim) loaded");
                return DaemonEmbedder::Candle(e);
            }
            Err(err) => {
                modelstat_log::log_warn!(
                    "embedder: BGE model not loadable at {} ({err}) — segmentation loses its \
                     topic-shift boundary and splits on size/time only until the background \
                     download lands",
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
                modelstat_log::log_info!("NER (redaction layer 2): candle BERT-NER loaded");
                return DaemonNer::Candle(n);
            }
            Err(err) => {
                modelstat_log::log_warn!(
                    "NER (redaction layer 2): model not loadable at {} ({err}) — \
                     redaction floor (layer 1) still applies; cloud/self-hosted flushes HOLD \
                     (fail-closed) until the background download lands",
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

    /// The regression guard for the bug this list exists to prevent: the BGE
    /// weights were never downloaded on ANY install because the loader knew about
    /// the model and the downloader did not. Everything now walks
    /// `ON_DEVICE_MODELS`, so the invariant is that the list is complete and each
    /// entry is coherent.
    #[test]
    fn every_model_the_daemon_loads_is_in_the_download_list() {
        let slots: Vec<ModelSlot> = ON_DEVICE_MODELS.iter().map(|m| m.slot).collect();
        assert!(
            slots.contains(&ModelSlot::Ner),
            "build_ner() loads a model that nothing downloads"
        );
        assert!(
            slots.contains(&ModelSlot::Embedder),
            "build_embedder() loads a model that nothing downloads — the original bug"
        );
        // One entry per slot: two entries feeding the same handle would mean one
        // silently wins and the other's download is dead weight.
        let mut seen = slots.clone();
        seen.sort_by_key(|s| format!("{s:?}"));
        seen.dedup();
        assert_eq!(seen.len(), slots.len(), "two models share one slot");
    }

    /// `status --json` keys are an API the tray and support scripts read.
    #[test]
    fn model_keys_are_stable_unique_and_machine_safe() {
        let keys: Vec<&str> = ON_DEVICE_MODELS.iter().map(|m| m.key).collect();
        assert_eq!(keys, vec!["redactor", "embedder"], "json keys changed");
        for entry in &ON_DEVICE_MODELS {
            assert!(
                !entry.key.contains(' ') && entry.key == entry.key.to_lowercase(),
                "key {:?} is not machine-safe",
                entry.key
            );
            assert!(!entry.label.is_empty());
            assert!(
                entry.model.weights_size_label.starts_with('~'),
                "{} has no size label for the user",
                entry.key
            );
        }
    }

    /// `missing_models` drives both the self-heal and `status`, so "present" must
    /// mean every file, not just the directory existing.
    #[test]
    fn a_model_counts_as_present_only_when_all_its_files_are() {
        let tmp = std::env::temp_dir().join(format!(
            "modelstat-models-test-{}",
            std::process::id() as u64
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let entry = &ON_DEVICE_MODELS[0];
        let dir = entry.model.dir(&tmp);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!entry.model.is_present(&tmp), "empty dir is not present");

        let files: Vec<_> = entry.model.files.iter().collect();
        for f in &files[..files.len() - 1] {
            std::fs::write(dir.join(f.local), b"x").unwrap();
        }
        assert!(
            !entry.model.is_present(&tmp),
            "a half-downloaded model must not read as present"
        );
        std::fs::write(dir.join(files[files.len() - 1].local), b"x").unwrap();
        assert!(entry.model.is_present(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

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
