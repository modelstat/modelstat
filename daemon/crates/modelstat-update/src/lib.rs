//! Self-update (feature §13) — npm-free, binary-native (plan D10).
//!
//! - [`auto_update`] — the `auto-update.json` preference + `MODELSTAT_AUTO_UPDATE`
//!   env pin (byte-parity with the TS daemon).
//! - [`marker`] — the `upgrade-in-progress.json` anti-stack marker (TTL 5 min,
//!   worker-pid liveness).
//! - [`release`] — the `daemon_release` heartbeat verdict + the GitHub-Releases
//!   archive URL for this target triple.
//! - [`perform`] — the binary self-replace state machine: download + verify →
//!   stop engine + quiesce → atomic swap of BOTH binaries (keep `.prev`) →
//!   `_install-service` → health probe → rollback on failure.
//!
//! The old npm/postinstall self-update (and its libnode-bricking class) is gone
//! (feature §22): `upgrade` replaces the running binaries in place from a GitHub
//! release and rolls back the `.prev` pair if the new build fails its post-swap
//! health probe.

pub mod auto_update;
pub mod marker;
pub mod perform;
pub mod release;

pub use auto_update::{
    auto_update_enabled, auto_update_pinned_by_env, set_stored_auto_update, stored_auto_update,
};
pub use marker::{clear_upgrade_marker, upgrade_in_progress, write_upgrade_marker};
pub use perform::{
    maybe_auto_update, perform_upgrade, rollback_pair, swap_pair, upgrade_now, AutoUpdateStep,
    UpgradeOutcome,
};
pub use release::{
    archive_url, resolve_latest_version, target_triple, DaemonRelease, ReleaseVerdict,
};

/// Serializes tests that mutate the process-global `MODELSTAT_HOME` /
/// `MODELSTAT_AUTO_UPDATE` env (cargo runs a crate's tests in parallel threads
/// of one process, so env writes would otherwise race).
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
