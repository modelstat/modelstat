//! The daemon's mutable config/identity context — the TS `apps/daemon/src/
//! config.ts` `state` object, as an explicit struct.
//!
//! **Parity note (structural):** the TS keeps this as a module-level singleton
//! with a cached identity + module-level recover backoff. Rust models it as an
//! explicit `Config` you construct and pass, with the identity cached behind a
//! `Mutex`. Behaviour is identical (write-through setters, same resolution
//! order); the struct just makes the five e2e scenarios drivable in one process
//! and keeps env-dependent state out of process globals. See daemon/PARITY.md.

use std::sync::Mutex;

use crate::identity::{load_identity, save_identity, DeviceIdentity};
use crate::machine_key::host_name;
use crate::state;
use crate::timefmt::now_iso;

/// Production API. Dev/e2e overrides via `DAEMON_API_URL`.
const DEFAULT_API_URL: &str = "https://modelstat.ai";

/// The localhost value `@modelstat/daemon@0.0.7` persisted on first run. Seeing
/// exactly this stored (with no env override) is treated as *unset* so upgrades
/// from 0.0.7 self-heal onto the production default. Port of TS
/// `LEGACY_LOCALHOST_API`. (Feature §22 notes the shim is dropped one release
/// after cutover; kept here for read-compat until then.)
const LEGACY_LOCALHOST_API: &str = "http://localhost:3010";

/// The fields a fresh self-register seeds. Mirror of the TS
/// `saveFreshIdentity` input.
pub struct FreshIdentity {
    pub device_uuid: String,
    pub device_id: String,
    pub bearer_token: String,
    pub claim_code: Option<String>,
    pub claim_url: Option<String>,
}

/// Mutable daemon config + cached identity. Cheap to construct (`Config::load`
/// reads `identity.json` once); construct one per CLI invocation, share one
/// across a single async flow.
pub struct Config {
    /// Compile-time `daemon-<semver>` version string (feature §5).
    version: &'static str,
    /// Cached identity, write-through on mutation. `None` = unpaired.
    identity: Mutex<Option<DeviceIdentity>>,
}

impl Config {
    /// Construct, loading `identity.json` from disk (honors `MODELSTAT_HOME`).
    pub fn load(version: &'static str) -> Self {
        Config {
            version,
            identity: Mutex::new(load_identity()),
        }
    }

    fn id(&self) -> std::sync::MutexGuard<'_, Option<DeviceIdentity>> {
        self.identity.lock().unwrap()
    }

    /// The `daemon-<semver>` version string.
    pub fn version(&self) -> &'static str {
        self.version
    }

    /// Resolution order: `DAEMON_API_URL` → stored override (unless it's the
    /// legacy localhost) → production default. Port of TS `state.apiUrl`.
    pub fn api_url(&self) -> String {
        if let Ok(v) = std::env::var("DAEMON_API_URL") {
            if !v.is_empty() {
                return v;
            }
        }
        let stored = state::get_api_url();
        if !stored.is_empty() && stored != LEGACY_LOCALHOST_API {
            return stored;
        }
        DEFAULT_API_URL.to_string()
    }

    /// True when `api_url()` resolves to the baked-in production default with no
    /// explicit override — the condition the CI/prod register guard checks so
    /// ephemeral runners don't pile up unclaimed device rows (feature §4).
    pub fn is_prod_default_api(&self) -> bool {
        if let Ok(v) = std::env::var("DAEMON_API_URL") {
            if !v.is_empty() {
                return false;
            }
        }
        let stored = state::get_api_url();
        if !stored.is_empty() && stored != LEGACY_LOCALHOST_API {
            return false;
        }
        true
    }

    // ── Identity (backed by identity.json) ──────────────────────────────

    pub fn identity(&self) -> Option<DeviceIdentity> {
        self.id().clone()
    }

    pub fn bearer(&self) -> Option<String> {
        self.id().as_ref().map(|i| i.bearer_token.clone())
    }

    pub fn device_id(&self) -> Option<String> {
        self.id().as_ref().map(|i| i.device_id.clone())
    }

    pub fn device_uuid(&self) -> Option<String> {
        self.id().as_ref().map(|i| i.device_uuid.clone())
    }

    pub fn claim_code(&self) -> Option<String> {
        self.id().as_ref().and_then(|i| i.claim_code.clone())
    }

    pub fn claim_url(&self) -> Option<String> {
        self.id().as_ref().and_then(|i| i.claim_url.clone())
    }

    /// Wipe (`None`) or write-through-update (`Some`) the in-memory bearer.
    /// `None` clears only the cache (the caller owns any backup); a fresh
    /// identity is then seeded by `save_fresh_identity`, exactly as the TS
    /// recover path does (`setBearer(null)` → `selfRegister` → `saveFreshIdentity`).
    pub fn set_bearer(&self, v: Option<String>) {
        let mut guard = self.id();
        match v {
            None => *guard = None,
            Some(token) => {
                if let Some(cur) = guard.as_mut() {
                    cur.bearer_token = token;
                    let _ = save_identity(cur);
                }
            }
        }
    }

    pub fn set_claim_code(&self, v: Option<String>) {
        let mut guard = self.id();
        if let Some(cur) = guard.as_mut() {
            if cur.claim_code != v {
                cur.claim_code = v;
                let _ = save_identity(cur);
            }
        }
    }

    pub fn set_claim_url(&self, v: Option<String>) {
        let mut guard = self.id();
        if let Some(cur) = guard.as_mut() {
            if cur.claim_url != v {
                cur.claim_url = v;
                let _ = save_identity(cur);
            }
        }
    }

    /// Seed a fresh identity after a successful self-register: writes
    /// `identity.json` atomically and updates the cache. Overwrites any existing
    /// file — the caller backs it up first if needed (`--fresh`). Port of TS
    /// `saveFreshIdentity`.
    pub fn save_fresh_identity(&self, meta: FreshIdentity) -> std::io::Result<()> {
        let id = DeviceIdentity {
            device_uuid: meta.device_uuid,
            device_id: meta.device_id,
            bearer_token: meta.bearer_token,
            claim_code: meta.claim_code,
            claim_url: meta.claim_url,
            hostname: host_name(),
            created_at: now_iso(),
            user_email: None,
            default_org_id: None,
        };
        save_identity(&id)?;
        *self.id() = Some(id);
        Ok(())
    }

    // ── Summarizer mode / self-hosted URL ───────────────────────────────

    /// The effective REDACTOR mode — where turns get scrubbed.
    /// `MODELSTAT_REDACTOR_MODE` overrides the stored choice, like its summariser
    /// twin. Independent on purpose: the common shape is scrub here, summarise
    /// there, and one setting could not express it.
    pub fn redactor_mode(&self) -> String {
        if let Some(m) =
            state::parse_redactor_mode(std::env::var("MODELSTAT_REDACTOR_MODE").ok().as_deref())
        {
            return m.to_string();
        }
        state::get_redactor_mode()
    }

    /// True when the redactor scrubs on THIS machine — the only mode where nothing
    /// unscrubbed can leave, and the reason it is the default.
    pub fn redacts_locally(&self) -> bool {
        self.redactor_mode() == "local"
    }

    /// The effective summarizer mode. `MODELSTAT_SUMMARIZER_MODE` overrides the
    /// persisted choice; an unset/garbage env value falls through. Port of TS
    /// `state.summarizerMode`.
    pub fn summarizer_mode(&self) -> String {
        if let Some(m) =
            state::parse_summarizer_mode(std::env::var("MODELSTAT_SUMMARIZER_MODE").ok().as_deref())
        {
            return m.to_string();
        }
        state::get_summarizer_mode()
    }

    /// True when `MODELSTAT_SUMMARIZER_MODE` is masking the stored choice — lets
    /// `mode`/`status` warn about it (feature §5).
    pub fn summarizer_mode_is_env_overridden(&self) -> bool {
        state::parse_summarizer_mode(std::env::var("MODELSTAT_SUMMARIZER_MODE").ok().as_deref())
            .is_some()
    }

    /// The effective self-hosted engine URL: `MODELSTAT_SUMMARIZER_URL`
    /// (new — feature §19, replaces the dropped `MODELSTAT_LLM_*` family)
    /// overrides the stored value.
    pub fn self_hosted_url(&self) -> String {
        if let Ok(v) = std::env::var("MODELSTAT_SUMMARIZER_URL") {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
        state::get_self_hosted_url()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env_lock;

    fn with_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _g = test_env_lock();
        std::env::remove_var("DAEMON_API_URL");
        std::env::remove_var("MODELSTAT_SUMMARIZER_MODE");
        std::env::remove_var("MODELSTAT_SUMMARIZER_URL");
        std::env::set_var("MODELSTAT_HOME", dir);
        let out = f();
        std::env::remove_var("MODELSTAT_HOME");
        out
    }

    #[test]
    fn api_url_resolution_order() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let c = Config::load("daemon-0.0.0");
            // 1. Nothing stored / no env ⇒ production default.
            assert_eq!(c.api_url(), DEFAULT_API_URL);
            assert!(c.is_prod_default_api());
            // 2. Stored override wins over default.
            state::set_api_url("https://stored.example").unwrap();
            assert_eq!(c.api_url(), "https://stored.example");
            assert!(!c.is_prod_default_api());
            // 3. Legacy localhost is treated as unset.
            state::set_api_url(LEGACY_LOCALHOST_API).unwrap();
            assert_eq!(c.api_url(), DEFAULT_API_URL);
            assert!(c.is_prod_default_api());
            // 4. Env override beats everything.
            std::env::set_var("DAEMON_API_URL", "https://env.example");
            assert_eq!(c.api_url(), "https://env.example");
            assert!(!c.is_prod_default_api());
            std::env::remove_var("DAEMON_API_URL");
        });
    }

    #[test]
    fn save_fresh_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let c = Config::load("daemon-0.0.0");
            assert!(c.bearer().is_none());
            c.save_fresh_identity(FreshIdentity {
                device_uuid: "u".into(),
                device_id: "dev_1".into(),
                bearer_token: "ds_live_abc".into(),
                claim_code: Some("code".into()),
                claim_url: Some("https://x/device/code".into()),
            })
            .unwrap();
            assert_eq!(c.bearer().as_deref(), Some("ds_live_abc"));
            assert_eq!(c.device_id().as_deref(), Some("dev_1"));
            // Persisted: a freshly loaded Config sees it too.
            let c2 = Config::load("daemon-0.0.0");
            assert_eq!(c2.device_id().as_deref(), Some("dev_1"));
            // set_bearer(None) clears the cache but not the file.
            c.set_bearer(None);
            assert!(c.bearer().is_none());
            assert!(Config::load("daemon-0.0.0").bearer().is_some());
        });
    }

    #[test]
    fn summarizer_mode_env_override() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let c = Config::load("daemon-0.0.0");
            assert_eq!(c.summarizer_mode(), "cloud");
            assert!(!c.summarizer_mode_is_env_overridden());
            state::set_summarizer_mode("local").unwrap();
            assert_eq!(c.summarizer_mode(), "local");
            std::env::set_var("MODELSTAT_SUMMARIZER_MODE", "self-hosted");
            assert_eq!(c.summarizer_mode(), "self-hosted");
            assert!(c.summarizer_mode_is_env_overridden());
            std::env::remove_var("MODELSTAT_SUMMARIZER_MODE");
        });
    }

    #[test]
    fn self_hosted_url_env_override() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let c = Config::load("daemon-0.0.0");
            state::set_self_hosted_url("https://stored.internal").unwrap();
            assert_eq!(c.self_hosted_url(), "https://stored.internal");
            std::env::set_var("MODELSTAT_SUMMARIZER_URL", "https://env.internal");
            assert_eq!(c.self_hosted_url(), "https://env.internal");
            std::env::remove_var("MODELSTAT_SUMMARIZER_URL");
        });
    }
}
