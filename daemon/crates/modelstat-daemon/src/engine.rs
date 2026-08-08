//! Engine + on-device model wiring for the daemon-main loop — the concrete
//! `Summarizer` / `Embedder` / `PiiModel` triple the scan + drain pipelines run
//! over, resolved from the install-time summarizer mode (feature §9.2, §9.5).
//!
//! Three moving parts:
//!   1. [`engine_base_url`] — where the summarizer protocol-v1 engine lives.
//!      `local` (and the unused-engine `cloud` default) reach the loopback engine
//!      at `http://127.0.0.1:<port>` (port from `summarizer.json`, default 4321);
//!      `self-hosted` is the same engine on the org's box (`selfHostedUrl`).
//!   2. [`build_embedder`] / [`build_redactor`] — the embedder + the layer-2 PII
//!      detector for the active REDACTOR mode. Both are role interfaces: which
//!      checkpoint backs them is a loader detail, and swapping a model must
//!      never ripple past its own build fn. Any load/config failure degrades
//!      LOUDLY to the fail-safe pair ([`NoEmbedder`] / [`UnavailableRedactor`]),
//!      which keeps every egress path correctly FAIL-CLOSED (detector
//!      unavailable ⇒ the flush holds, never floor-only egress).
//!
//!   3. [`RemoteConfig`] — the server-delivered config kinds (`policies`,
//!      `calibration`) and what adopting one means. Same shape as the model
//!      handles: a value the daemon holds, swapped in place when a newer one
//!      lands, never blocking anything when the server can't be reached.
//!
//! The engine binary itself (llama.cpp) is NEVER linked here — the collector only
//! ever speaks the HTTP protocol to it (plan D4). No `modelstat-llm` dependency.

use std::path::PathBuf;

use modelstat_ingest::remote_config::{ConfigChannel, Versioned};
use modelstat_ingest::{home_path, Config};
use modelstat_pipeline::{
    install_calibration, Calibration, Embedder, NoEmbedder, CALIBRATION_CONFIG_KIND,
};
use modelstat_redact::{
    compile_policy_patterns, install_policy_patterns, PiiModel, PiiToken, RedactionPolicyBundle,
    UnavailableRedactor, POLICIES_BUNDLED_FALLBACK, POLICIES_CONFIG_KIND,
};

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
/// `~/.modelstat/models`) — one cache for `connect` + the daemon so model
/// weights download once and survive upgrades (§9.5). The `hf/<name>`
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
    Redactor,
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
        model: &modelstat_download::PRIVACY_FILTER,
        key: "redactor",
        label: "PII redactor",
        slot: ModelSlot::Redactor,
    },
    OnDeviceModel {
        model: &modelstat_download::BGE_SMALL,
        key: "embedder",
        label: "embedder",
        slot: ModelSlot::Embedder,
    },
];

/// The models this install actually needs, given where redaction runs: a
/// remote-redactor device does NOT need the ~900 MB Privacy Filter on disk —
/// that is half the point of the cloud default — so the self-heal must not
/// download it and `status` must not report it missing.
pub fn required_models(redacts_locally: bool) -> Vec<&'static OnDeviceModel> {
    ON_DEVICE_MODELS
        .iter()
        .filter(|m| redacts_locally || m.slot != ModelSlot::Redactor)
        .collect()
}

/// The required models whose files are NOT all on disk. Cheap (a `stat` per
/// file), side-effect-free — what `status` reports and what the self-heal acts
/// on.
pub fn missing_models(redacts_locally: bool) -> Vec<&'static OnDeviceModel> {
    let dir = models_cache_dir();
    required_models(redacts_locally)
        .into_iter()
        .filter(|m| !m.model.is_present(&dir))
        .collect()
}

/// Delete cached model directories no entry in [`ON_DEVICE_MODELS`] claims,
/// returning what was removed.
///
/// Replacing a model used to leave the old weights on disk forever: swapping the
/// BERT redactor for Privacy Filter left 414 MB of `bert-base-NER` sitting in the
/// cache of every install, downloaded once and never read again. Nothing was ever
/// going to notice, because nothing looked.
///
/// Keyed on [`ON_DEVICE_MODELS`] rather than a list of names to delete, so this
/// cannot go stale: whatever the daemon does not load is, by definition, garbage.
/// That makes it destructive by construction, so it deletes ONLY directories
/// directly under `<models>/hf/` — the ones this code created — and never
/// recurses anywhere else.
pub fn prune_stale_models() -> Vec<String> {
    let hf = models_cache_dir().join("hf");
    let Ok(entries) = std::fs::read_dir(&hf) else {
        return Vec::new(); // no cache yet — nothing to prune
    };
    let keep: std::collections::BTreeSet<&str> =
        ON_DEVICE_MODELS.iter().map(|m| m.model.dir_name).collect();
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.contains(name.as_str()) {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => removed.push(name),
            Err(e) => modelstat_log::log_warn!(
                "could not remove the unused model cache {}: {e}",
                entry.path().display()
            ),
        }
    }
    removed
}

/// Pre-warm the layer-2 PII detector into the shared cache (§9.5). Returns
/// whether it is now present. `connect`/`mode` call it so the first scan runs at
/// full redaction quality; the bounded [`RetryPolicy::interactive`] keeps a human
/// from waiting forever, and the daemon's self-heal finishes what this can't.
pub async fn ensure_redactor_model() -> bool {
    ensure_model(&modelstat_download::PRIVACY_FILTER, "PII redactor").await
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
/// missing forever, and the user had no way to know. A missing PII detector is
/// worse than a quality loss: flushes fail closed and HOLD, so the daemon
/// uploads nothing at all until it lands.
///
/// Returns the models it successfully fetched, so the caller can hot-swap them in
/// without a restart. Uses [`RetryPolicy::forever`] — nobody is waiting, and an
/// offline machine simply resumes when the network returns.
pub async fn heal_missing_models(redacts_locally: bool) -> Vec<&'static OnDeviceModel> {
    let missing = missing_models(redacts_locally);
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

/// The daemon's layer-2 PII detector for the active REDACTOR mode: the
/// on-device model (`local`), the serving model behind modelstat's
/// `/v1/redact/*` (`cloud`) or an org's own endpoint (`self-hosted`) — or the
/// fail-closed [`UnavailableRedactor`] when the mode's backend can't even be
/// constructed. Whatever the variant, "could not classify" makes the flush
/// HOLD (fail-closed, §9.5) rather than ship less-redacted content; the
/// layer-1 floor has run on-device before any of these ever see a byte.
pub enum DaemonRedactor {
    /// Detector inactive — only the deterministic floor (layer 1) applies, and
    /// every flush that must classify holds (no egress).
    Unavailable(UnavailableRedactor),
    /// A remote span classifier speaking the `/v1/redact` protocol — cloud or
    /// self-hosted, same client. Behind the span cache so repeated texts cost a
    /// hash lookup, not a round-trip.
    Remote(modelstat_redact::CachedNer<modelstat_ingest::redactor_client::RemoteRedactor>),
    /// OpenAI Privacy Filter over ONNX Runtime — the PII detector (§9.5) —
    /// behind the span cache, so a text the model already classified (repeated
    /// tool output, or the whole corpus on a version-bump re-scan) skips
    /// inference. The cache stores raw model answers only; with it cold, absent,
    /// or disabled the behaviour is exactly the bare model's.
    #[cfg(feature = "onnx")]
    PrivacyFilter(modelstat_redact::CachedNer<modelstat_redact::privacy_filter::PrivacyFilter>),
}

impl PiiModel for DaemonRedactor {
    fn classify(&self, text: &str) -> Option<Vec<PiiToken>> {
        match self {
            DaemonRedactor::Unavailable(n) => n.classify(text),
            DaemonRedactor::Remote(n) => n.classify(text),
            #[cfg(feature = "onnx")]
            DaemonRedactor::PrivacyFilter(n) => n.classify(text),
        }
    }

    fn classify_many(&self, texts: &[String]) -> Option<Vec<Vec<PiiToken>>> {
        match self {
            DaemonRedactor::Unavailable(n) => n.classify_many(texts),
            DaemonRedactor::Remote(n) => n.classify_many(texts),
            #[cfg(feature = "onnx")]
            DaemonRedactor::PrivacyFilter(n) => n.classify_many(texts),
        }
    }
}

/// The span cache under `fingerprint`, or `None` when it can't be opened — the
/// cache may only ever make redaction faster, so every failure here degrades to
/// "no cache", never to "no redaction".
///
/// `MODELSTAT_SPAN_CACHE=off` disables it; `MODELSTAT_SPAN_CACHE_MAX_MB`
/// resizes it (default 512). Local and remote answers share the one file —
/// their fingerprints differ, so their keys can never collide.
fn open_span_store(fingerprint: &str) -> Option<modelstat_redact::SpanStore> {
    if matches!(
        std::env::var("MODELSTAT_SPAN_CACHE").ok().as_deref(),
        Some("off") | Some("0") | Some("false")
    ) {
        return None;
    }
    let max_bytes = std::env::var("MODELSTAT_SPAN_CACHE_MAX_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(modelstat_redact::span_cache::DEFAULT_MAX_BYTES);
    match modelstat_redact::SpanStore::open(
        &home_path("span-cache.sqlite3"),
        fingerprint,
        max_bytes,
    ) {
        Ok(store) => Some(store),
        Err(e) => {
            modelstat_log::log_warn!(
                "span cache could not open ({e}) — redaction runs uncached at full model cost"
            );
            None
        }
    }
}

/// Name the exact model the cache's answers come from: a digest over the
/// checkpoint config, every model file's size, a head+tail sample of the big
/// weights sidecar, and the recall bias in force. There is no pinned upstream
/// revision to lean on (the downloader tracks the repo), so identity comes from
/// the bytes on disk. `None` — files unreadable — means "cannot say", and an
/// unnameable model gets no cache rather than a guessed key.
#[cfg(feature = "onnx")]
fn redactor_fingerprint(model_dir: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(std::fs::read(model_dir.join("config.json")).ok()?);
    for entry in modelstat_download::PRIVACY_FILTER.files {
        let meta = std::fs::metadata(model_dir.join(entry.local)).ok()?;
        h.update(entry.local.as_bytes());
        h.update(meta.len().to_le_bytes());
    }
    // 64 KiB off each end of the weights sidecar: enough that swapped weights
    // can't share a fingerprint by size alone, cheap enough for every boot.
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(model_dir.join("onnx").join("model_q4.onnx_data")).ok()?;
        let len = f.metadata().ok()?.len();
        let mut buf = vec![0u8; 64 * 1024];
        let n = f.read(&mut buf).ok()?;
        h.update(&buf[..n]);
        f.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)))
            .ok()?;
        let n = f.read(&mut buf).ok()?;
        h.update(&buf[..n]);
    }
    Some(format!(
        "local:{:x}:bias={}",
        h.finalize(),
        modelstat_redact::privacy_filter::recall_bias()
    ))
}

/// Build the redactor for the active REDACTOR mode.
///
/// `cloud` / `self-hosted` construct the remote client (span-cached under the
/// server's version-bearing model id, probed once here); `local` loads OpenAI
/// Privacy Filter from the shared cache when built `--features onnx` and its
/// weights are present. Every failure path lands on [`UnavailableRedactor`] — which
/// the redaction floor still backstops, and which keeps EVERY mode fail-closed,
/// since an uploaded abstract is egress too.
pub fn build_redactor(config: &Config) -> DaemonRedactor {
    match config.redactor_mode().as_str() {
        "cloud" => {
            let Some(bearer) = config.bearer() else {
                // Unpaired process (tests, a fresh install mid-enroll): nothing
                // can be classified remotely without credentials, so hold.
                modelstat_log::log_warn!(
                    "cloud redactor needs a paired device — flushes hold until enrollment"
                );
                return DaemonRedactor::Unavailable(UnavailableRedactor);
            };
            build_remote_redactor(&config.api_url(), Some(bearer), "cloud")
        }
        "self-hosted" => {
            let url = config.redactor_url();
            if url.trim().is_empty() {
                modelstat_log::log_error!(
                    "self-hosted redactor has no endpoint URL — set one with \
                     `modelstat redactor self-hosted --url <URL>`; flushes hold until then"
                );
                return DaemonRedactor::Unavailable(UnavailableRedactor);
            }
            build_remote_redactor(&url, None, "self-hosted")
        }
        _ => build_local_redactor(),
    }
}

/// The remote span classifier for `base`, span-cached when the endpoint can
/// name its weights. The healthz probe is best-effort: an unreachable endpoint
/// still yields a working client (classification holds + retries), it just
/// runs uncached until a boot finds the endpoint up.
fn build_remote_redactor(base: &str, bearer: Option<String>, label: &str) -> DaemonRedactor {
    let client = modelstat_ingest::redactor_client::RemoteRedactor::new(base, bearer);
    let store = match client.healthz_blocking() {
        Some(h) if h.model_loaded => open_span_store(&format!("remote:{}", h.model)),
        Some(_) => {
            modelstat_log::log_info!(
                "{label} redactor at {base} is still loading its model — flushes hold until ready"
            );
            None
        }
        None => {
            modelstat_log::log_warn!(
                "{label} redactor at {base} is unreachable — flushes hold + retry; \
                 span cache stays off until a boot finds it up"
            );
            None
        }
    };
    modelstat_log::log_info!(
        "redactor (layer 2): {label} span classifier at {base}{}",
        if store.is_some() {
            ", span cache on"
        } else {
            ", span cache off"
        }
    );
    DaemonRedactor::Remote(modelstat_redact::CachedNer::new(client, store))
}

/// The on-device Privacy Filter (`local` mode), or the fail-closed fallback.
fn build_local_redactor() -> DaemonRedactor {
    #[cfg(feature = "onnx")]
    {
        let dir = model_dir("MODELSTAT_REDACTOR_MODEL_DIR", "privacy-filter");
        match modelstat_redact::privacy_filter::PrivacyFilter::load(&dir) {
            Ok(n) => {
                let store = redactor_fingerprint(&dir)
                    .as_deref()
                    .and_then(open_span_store);
                modelstat_log::log_info!(
                    "redactor (layer 2): OpenAI Privacy Filter loaded (PII spans, on-device{})",
                    if store.is_some() {
                        ", span cache on"
                    } else {
                        ", span cache off"
                    }
                );
                return DaemonRedactor::PrivacyFilter(modelstat_redact::CachedNer::new(n, store));
            }
            Err(err) => {
                modelstat_log::log_warn!(
                    "redactor (layer 2): model not loadable at {} ({err}) — \
                     redaction floor (layer 1) still applies; flushes HOLD \
                     (fail-closed) until the background download lands",
                    dir.display()
                );
            }
        }
    }
    DaemonRedactor::Unavailable(UnavailableRedactor)
}

// ── Server-delivered config ──────────────────────────────────────────────────

/// The config kinds this daemon knows, and what adopting one MEANS.
///
/// The channel itself ([`modelstat_ingest::remote_config`]) is vocabulary-free:
/// it fetches, shape-validates, version-gates and caches JSON. Everything
/// specific to a kind lives here — the validator, and the one call that puts a
/// new payload into force. Each kind installs into the crate that owns the
/// semantics rather than being threaded down through the scan:
///
///   - `policies` → [`modelstat_redact`]'s process-wide additive augment, so
///     EVERY floor call site gets it and no future one can miss it by omission.
///     Additive by construction, so a wrong payload can only redact more.
///   - `calibration` → [`modelstat_pipeline`]'s segmentation thresholds,
///     clamped on parse, applied to the next scan.
///
/// A third kind is a validator plus an install line; nothing below this changes.
pub struct RemoteConfig {
    policies: ConfigChannel<RedactionPolicyBundle>,
    calibration: ConfigChannel<Calibration>,
}

fn validate_policies(raw: &str) -> Option<Versioned<RedactionPolicyBundle>> {
    let bundle: RedactionPolicyBundle = serde_json::from_str(raw).ok()?;
    Some(Versioned {
        version: bundle.version,
        value: bundle,
    })
}

fn validate_calibration(raw: &str) -> Option<Versioned<Calibration>> {
    Calibration::from_payload(raw).map(|(version, value)| Versioned { version, value })
}

impl RemoteConfig {
    /// Seed every kind from its disk cache (or the compiled-in default) and put
    /// it into force. No network: this runs on the boot path, and a daemon that
    /// starts offline must come back on the last config it saw.
    pub fn load() -> Self {
        let this = RemoteConfig {
            policies: ConfigChannel::new(
                POLICIES_CONFIG_KIND,
                validate_policies,
                Versioned {
                    version: POLICIES_BUNDLED_FALLBACK.version,
                    value: POLICIES_BUNDLED_FALLBACK,
                },
            ),
            calibration: ConfigChannel::new(
                CALIBRATION_CONFIG_KIND,
                validate_calibration,
                Versioned {
                    version: 0,
                    value: Calibration::default(),
                },
            ),
        };
        this.apply_policies();
        this.apply_calibration();
        this
    }

    /// Fetch every kind once and put whatever moved into force. Best-effort by
    /// contract: a failure keeps the cached (or bundled) payload, so this can
    /// run on a timer and be ignored.
    pub async fn refresh(&self, api_url: &str) {
        if self.policies.refresh(api_url).await.is_some() {
            self.apply_policies();
        }
        if self.calibration.refresh(api_url).await.is_some() {
            self.apply_calibration();
        }
    }

    fn apply_policies(&self) {
        let held = self.policies.current();
        let compiled = compile_policy_patterns(&held.value);
        let skipped = held.value.patterns.len() - compiled.len();
        if skipped > 0 {
            // Never fatal: one unusable pattern must not cost the other twenty.
            modelstat_log::log_warn!(
                "policies v{}: {skipped} pattern(s) unusable and skipped — \
                 the floor plus the remaining {} still apply",
                held.version,
                compiled.len()
            );
        }
        modelstat_log::log_info!(
            "redaction augment: {} additive pattern(s) from {} v{} (the floor always applies)",
            compiled.len(),
            self.policies.kind(),
            held.version
        );
        install_policy_patterns(compiled);
    }

    fn apply_calibration(&self) {
        let held = self.calibration.current();
        modelstat_log::log_info!(
            "segmentation thresholds: {} v{} ({} min gap, {} turns, {} min, {} chars, topic > {})",
            self.calibration.kind(),
            held.version,
            held.value.time_gap_ms / 60_000,
            held.value.max_turns,
            held.value.max_duration_ms / 60_000,
            held.value.max_content_chars,
            held.value.topic_threshold
        );
        install_calibration(held.value.clone());
    }
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
            slots.contains(&ModelSlot::Redactor),
            "build_redactor() loads a model that nothing downloads"
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

    /// The cleanup keeps what the daemon loads and deletes what it does not — the
    /// 414 MB of superseded BERT weights every install was carrying.
    #[test]
    fn pruning_removes_superseded_models_and_keeps_the_live_ones() {
        let tmp = std::env::temp_dir().join(format!("modelstat-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let hf = tmp.join("hf");
        std::fs::create_dir_all(&hf).unwrap();
        // One live model, one superseded one, and a stray file that is not a
        // model dir at all.
        let live = ON_DEVICE_MODELS[0].model.dir_name;
        std::fs::create_dir_all(hf.join(live)).unwrap();
        std::fs::create_dir_all(hf.join("bert-base-NER")).unwrap();
        std::fs::write(hf.join("bert-base-NER").join("model.safetensors"), b"old").unwrap();
        std::fs::write(hf.join("notes.txt"), b"x").unwrap();

        let removed = with_env(
            &[("MODELSTAT_MODELS_DIR", Some(tmp.to_str().unwrap()))],
            prune_stale_models,
        );
        assert_eq!(removed, vec!["bert-base-NER"]);
        assert!(
            !hf.join("bert-base-NER").exists(),
            "the old weights are gone"
        );
        assert!(hf.join(live).exists(), "a live model must never be pruned");
        assert!(
            hf.join("notes.txt").exists(),
            "only directories are pruned — never loose files"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A missing cache is the first-boot case, not an error.
    #[test]
    fn pruning_an_absent_cache_is_a_no_op() {
        let tmp = std::env::temp_dir().join(format!("modelstat-prune-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let removed = with_env(
            &[("MODELSTAT_MODELS_DIR", Some(tmp.to_str().unwrap()))],
            prune_stale_models,
        );
        assert!(removed.is_empty());
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

        // Parents created explicitly: a model's local path can be NESTED
        // (`onnx/model_q4.onnx`), which the real downloader handles and this fake
        // must too.
        let touch = |rel: &str| {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, b"x").unwrap();
        };
        let files: Vec<_> = entry.model.files.iter().collect();
        for f in &files[..files.len() - 1] {
            touch(f.local);
        }
        assert!(
            !entry.model.is_present(&tmp),
            "a half-downloaded model must not read as present"
        );
        touch(files[files.len() - 1].local);
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
        // Forced-local without weights → fail-closed Unavailable, never a
        // silent pass-through. (An empty temp home also has no identity, so the
        // cloud default would be Unavailable too — pin the local branch, which
        // is the one with a model to miss.)
        let tmp =
            std::env::temp_dir().join(format!("modelstat-redactor-build-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let built = with_env(
            &[
                ("MODELSTAT_REDACTOR_MODE", Some("local")),
                ("MODELSTAT_HOME", Some(tmp.to_str().unwrap())),
            ],
            || build_redactor(&Config::load("daemon-test")),
        );
        assert!(matches!(built, DaemonRedactor::Unavailable(_)) || cfg!(feature = "onnx"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn remote_models_are_not_required_on_disk() {
        // The point of the cloud default: no ~900 MB download. The detector
        // slot drops out of the required list; the embedder never does.
        let remote: Vec<_> = required_models(false).iter().map(|m| m.slot).collect();
        assert!(!remote.contains(&ModelSlot::Redactor));
        assert!(remote.contains(&ModelSlot::Embedder));
        let local: Vec<_> = required_models(true).iter().map(|m| m.slot).collect();
        assert!(local.contains(&ModelSlot::Redactor));
    }

    #[test]
    fn self_hosted_without_a_url_is_fail_closed() {
        let tmp =
            std::env::temp_dir().join(format!("modelstat-redactor-sh-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let built = with_env(
            &[
                ("MODELSTAT_REDACTOR_MODE", Some("self-hosted")),
                ("MODELSTAT_REDACTOR_URL", None),
                ("MODELSTAT_HOME", Some(tmp.to_str().unwrap())),
            ],
            || build_redactor(&Config::load("daemon-test")),
        );
        assert!(
            matches!(built, DaemonRedactor::Unavailable(_)),
            "a remote mode with nowhere to send must hold, not pass through"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
