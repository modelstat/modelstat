//! Per-install runtime state — `~/.modelstat/state.json`. Port of the TS
//! `apps/daemon/src/runtime-state.ts` (feature §19). Non-secret bookkeeping:
//! file cursors, the API-URL override, counters, the processing-version marker,
//! degrade/reconcile/reship state, and the summarizer mode + self-hosted URL.
//!
//! **Dropped field:** `selfHostedModel` is read-tolerated, ignored, and dropped
//! on the next write (feature §19/§23; BYO-endpoint model ids are gone). The
//! legacy golden `state_legacy.json` carries it so this reader is tested against
//! it. Missing/corrupt ⇒ defaults (forces a full first scan).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::paths::{ensure_home, home_path};

/// Where each session gets summarised (feature §9). Redaction stays client-side
/// in EVERY mode; only the summarisation LOCATION differs.
pub const SUMMARIZER_MODES: [&str; 3] = ["cloud", "local", "self-hosted"];

/// The install default: cloud (no local model, server-side summarisation).
pub const DEFAULT_SUMMARIZER_MODE: &str = "cloud";

/// Narrow an arbitrary string to a valid summarizer mode (trim + lowercase +
/// membership), else `None`. Port of TS `parseSummarizerMode`.
pub fn parse_summarizer_mode(v: Option<&str>) -> Option<&'static str> {
    let s = v.unwrap_or("").trim().to_lowercase();
    SUMMARIZER_MODES.into_iter().find(|m| *m == s)
}

/// One per-file scan cursor. `tailHash` is camelCase on the wire (the TS field).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCursor {
    pub size: u64,
    pub mtime: i64,
    pub tail_hash: String,
    /// Watermark for sources whose records are NOT positional — Cursor's
    /// `state.vscdb`, where a chat message is a key/value row, not a byte range.
    /// Their floor is "already shipped through this instant" instead of "already
    /// shipped below this offset". Absent (and omitted from `state.json`) for
    /// every transcript file, so the serialized shape is unchanged for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shipped_through_ms: Option<i64>,
}

/// The runtime state as HELD and SERIALIZED. Field order = the `state_v16.json`
/// golden. Note the deliberate spelling split, matching the TS field names
/// exactly: `summariser*` (British 's') vs `summarizerMode` (American 'z').
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeState {
    pub api_url: String,
    pub cursor: BTreeMap<String, FileCursor>,
    pub segments_sent: i64,
    pub processing_version: Option<i64>,
    pub reconcile_cache: Value,
    #[serde(rename = "summariserDegraded")]
    pub summariser_degraded: bool,
    #[serde(rename = "summariserRecoveryAt")]
    pub summariser_recovery_at: i64,
    pub reship_state: Value,
    pub summarizer_mode: String,
    pub self_hosted_url: String,
}

impl Default for RuntimeState {
    fn default() -> Self {
        RuntimeState {
            api_url: String::new(),
            cursor: BTreeMap::new(),
            segments_sent: 0,
            processing_version: None,
            reconcile_cache: json!({}),
            summariser_degraded: false,
            summariser_recovery_at: 0,
            reship_state: json!({}),
            summarizer_mode: DEFAULT_SUMMARIZER_MODE.to_string(),
            self_hosted_url: String::new(),
        }
    }
}

/// Tolerant read target — every field optional, and `selfHostedModel` is simply
/// not declared, so serde ignores it (the drop-on-next-write behavior).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialState {
    api_url: Option<String>,
    cursor: Option<BTreeMap<String, FileCursor>>,
    segments_sent: Option<i64>,
    processing_version: Option<i64>,
    reconcile_cache: Option<Value>,
    #[serde(rename = "summariserDegraded")]
    summariser_degraded: Option<bool>,
    #[serde(rename = "summariserRecoveryAt")]
    summariser_recovery_at: Option<i64>,
    reship_state: Option<Value>,
    summarizer_mode: Option<String>,
    self_hosted_url: Option<String>,
}

/// `~/.modelstat/state.json` (honors `MODELSTAT_HOME`).
pub fn state_path() -> PathBuf {
    home_path("state.json")
}

/// Load + validate. A hand-edited/garbage `summarizerMode` falls back to the
/// default rather than driving the pipeline off a bogus branch (TS `load()`).
/// Missing/corrupt ⇒ fresh defaults (cursors empty ⇒ processingVersion=null
/// forces a re-scan — exactly what we want on a new/relocated install).
pub fn load_state() -> RuntimeState {
    let Ok(raw) = std::fs::read_to_string(state_path()) else {
        return RuntimeState::default();
    };
    let Ok(obj) = serde_json::from_str::<PartialState>(&raw) else {
        return RuntimeState::default();
    };
    let d = RuntimeState::default();
    RuntimeState {
        api_url: obj.api_url.unwrap_or(d.api_url),
        cursor: obj.cursor.unwrap_or(d.cursor),
        segments_sent: obj.segments_sent.unwrap_or(d.segments_sent),
        processing_version: obj.processing_version,
        reconcile_cache: obj.reconcile_cache.unwrap_or(d.reconcile_cache),
        summariser_degraded: obj.summariser_degraded.unwrap_or(d.summariser_degraded),
        summariser_recovery_at: obj
            .summariser_recovery_at
            .unwrap_or(d.summariser_recovery_at),
        reship_state: obj.reship_state.unwrap_or(d.reship_state),
        summarizer_mode: parse_summarizer_mode(obj.summarizer_mode.as_deref())
            .unwrap_or(DEFAULT_SUMMARIZER_MODE)
            .to_string(),
        self_hosted_url: obj.self_hosted_url.unwrap_or(d.self_hosted_url),
    }
}

/// Atomic write (`<file>.<pid>.tmp` + rename, `0600`), matching the TS `persist`.
/// Note: `selfHostedModel` is never written — it is dropped here on every save.
pub fn save_state(s: &RuntimeState) -> std::io::Result<()> {
    ensure_home()?;
    let path = state_path();
    let tmp = crate::atomic::with_pid_tmp(&path);
    let json = serde_json::to_string_pretty(s).expect("RuntimeState always serializes");
    std::fs::write(&tmp, json)?;
    crate::atomic::set_file_0600(&tmp);
    std::fs::rename(&tmp, &path)?;
    crate::atomic::set_file_0600(&path);
    Ok(())
}

// ── Convenience getters/setters (load → modify → save), the M1 surface. ──
// Cursor mutation + the reconcile/reship/degrade bookkeeping are scan-loop
// concerns and land with M4; the struct round-trips them losslessly meanwhile.

/// The stored API-URL override (empty ⇒ env / production default).
pub fn get_api_url() -> String {
    load_state().api_url
}
pub fn set_api_url(v: &str) -> std::io::Result<()> {
    let mut s = load_state();
    s.api_url = v.to_string();
    save_state(&s)
}

/// The persisted summarizer mode (always a valid value — `load_state` validates).
pub fn get_summarizer_mode() -> String {
    load_state().summarizer_mode
}
pub fn set_summarizer_mode(v: &str) -> std::io::Result<()> {
    let mut s = load_state();
    s.summarizer_mode = v.to_string();
    save_state(&s)
}

/// The stored self-hosted engine URL (empty unless mode is `self-hosted`). The
/// model id is gone with BYO endpoints (feature §19).
pub fn get_self_hosted_url() -> String {
    load_state().self_hosted_url
}
pub fn set_self_hosted_url(url: &str) -> std::io::Result<()> {
    let mut s = load_state();
    s.self_hosted_url = url.to_string();
    save_state(&s)
}

pub fn get_processing_version() -> Option<i64> {
    load_state().processing_version
}
pub fn set_processing_version(v: i64) -> std::io::Result<()> {
    let mut s = load_state();
    s.processing_version = Some(v);
    save_state(&s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env_lock;

    fn with_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _g = test_env_lock();
        std::env::set_var("MODELSTAT_HOME", dir);
        let out = f();
        std::env::remove_var("MODELSTAT_HOME");
        out
    }

    fn golden(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/../modelstat-wire/tests/golden/file_formats/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn parse_mode_validates() {
        assert_eq!(parse_summarizer_mode(Some(" Cloud ")), Some("cloud"));
        assert_eq!(
            parse_summarizer_mode(Some("self-hosted")),
            Some("self-hosted")
        );
        assert_eq!(parse_summarizer_mode(Some("bogus")), None);
        assert_eq!(parse_summarizer_mode(None), None);
    }

    #[test]
    fn reads_v16_golden_and_reserializes_identically() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            crate::paths::ensure_home().unwrap();
            std::fs::write(state_path(), golden("state_v16.json")).unwrap();
            let s = load_state();
            assert_eq!(s.processing_version, Some(16));
            assert_eq!(s.summarizer_mode, "cloud");
            assert_eq!(s.segments_sent, 42);
            assert_eq!(s.cursor.len(), 1);
            // Re-serialize → semantically identical to the golden.
            let mine: Value = serde_json::to_value(&s).unwrap();
            let want: Value = serde_json::from_str(&golden("state_v16.json")).unwrap();
            assert_eq!(mine, want);
        });
    }

    #[test]
    fn reads_legacy_and_drops_self_hosted_model() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            crate::paths::ensure_home().unwrap();
            std::fs::write(state_path(), golden("state_legacy.json")).unwrap();
            let s = load_state();
            // Legacy fields read tolerantly.
            assert_eq!(s.processing_version, Some(15));
            assert_eq!(s.summarizer_mode, "self-hosted");
            assert_eq!(s.self_hosted_url, "https://llm.acme.internal");
            assert!(s.summariser_degraded);
            // selfHostedModel is dropped on write.
            save_state(&s).unwrap();
            let written = std::fs::read_to_string(state_path()).unwrap();
            assert!(
                !written.contains("selfHostedModel"),
                "selfHostedModel must be dropped on the next write"
            );
            // apiUrl legacy value is preserved at the store layer (the localhost
            // reinterpretation is a Config-layer concern, not the store's).
            assert_eq!(s.api_url, "http://localhost:3010");
        });
    }

    #[test]
    fn missing_file_is_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let s = load_state();
            assert_eq!(s, RuntimeState::default());
            assert_eq!(s.processing_version, None);
            assert_eq!(s.summarizer_mode, "cloud");
        });
    }

    #[test]
    fn setters_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            set_api_url("https://dev.example").unwrap();
            set_summarizer_mode("local").unwrap();
            set_self_hosted_url("https://llm.internal").unwrap();
            set_processing_version(16).unwrap();
            assert_eq!(get_api_url(), "https://dev.example");
            assert_eq!(get_summarizer_mode(), "local");
            assert_eq!(get_self_hosted_url(), "https://llm.internal");
            assert_eq!(get_processing_version(), Some(16));
        });
    }
}
