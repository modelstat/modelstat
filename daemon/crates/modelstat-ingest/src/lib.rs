//! `modelstat-ingest` — the daemon's identity / config / device-API foundation
//! (M1) and, later, the IngestClient + backfill reconcile (M4). See plan §5.
//!
//! It is the lowest I/O crate in the graph (below both `modelstat-daemon` and
//! `modelstat-cli`) and `recoverIdentity` — spec-assigned here — needs the
//! identity store + config, so the whole impure foundation lives here:
//!
//! - [`paths`] — `MODELSTAT_HOME`-aware home + path resolution.
//! - [`machine_key`] — OS/hardware id probes, the fallback key (now honoring
//!   `MODELSTAT_HOME`, §23 fix), the device UUID, and the register/heartbeat
//!   fingerprint.
//! - [`identity`] — the `identity.json` store (atomic, 0600, backups).
//! - [`state`] — the `state.json` store (drops the legacy `selfHostedModel`).
//! - [`config`] — the mutable config/identity context (TS `state`).
//! - [`device_api`] — register / devices-me / recover / heartbeat + the shared
//!   retry matrix.
//!
//! Everything is a byte-for-byte port of the TS `apps/daemon/src/*` sources so
//! existing devices never re-enroll (feature §4, §21.9). See daemon/PARITY.md.

mod atomic;
pub mod config;
pub mod device_api;
pub mod identity;
pub mod machine_key;
pub mod paths;
pub mod state;
pub mod timefmt;

// Flat re-exports for the common (CLI + daemon) surface.
pub use config::{Config, FreshIdentity};
pub use device_api::{
    DeviceApi, DeviceMeError, DeviceMeResponse, HeartbeatResponse, SelfRegisterResponse,
};
pub use identity::{
    backup_identity, has_identity_file, identity_path, load_identity, save_identity, DeviceIdentity,
};
pub use machine_key::{build_fingerprint, intended_device_uuid, machine_key_source};
pub use paths::{home_path, logs_dir, modelstat_home};
pub use state::{state_path, RuntimeState};

/// Serialize the env-mutating tests across this crate. `cargo test` runs a
/// crate's tests on multiple threads in one process, so tests that flip
/// `MODELSTAT_HOME` / `DAEMON_API_URL` / etc. must not interleave. Poison-safe:
/// a panicking test releases the lock instead of wedging the suite.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
