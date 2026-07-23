//! The tray's adopt / spawn / replace decision (§15) — a port of
//! `apps/daemon/src/supervise.ts`. `modelstat _daemon-health` prints this JSON;
//! the Swift tray stays a thin "run command, switch on decision".
//!
//!   - **adopt**   — a live daemon owns the lock and is heartbeating (or is a
//!                   fresh boot inside the grace window): use it as-is.
//!   - **spawn**   — no lock, or the owner is dead: start a new daemon.
//!   - **replace** — a live owner is wedged (stale heartbeat past the grace) OR
//!                   is an OLD version after an upgrade re-staged the bundle
//!                   (it survives launchd's group kill + never picks up new code).

use serde_json::{json, Value};

use crate::lock::{daemon_lock_path, is_process_alive, read_daemon_lock, LockMeta};

/// A heartbeat this fresh (≤2 min) means the live owner is healthy → adopt it.
pub const STATUS_FRESH_MS: i64 = 120_000;
/// A fresh boot hasn't written a heartbeat yet; give a lock this young the
/// benefit of the doubt (≤90s) rather than replacing a daemon that's still coming
/// up.
pub const BOOT_GRACE_MS: i64 = 90_000;

/// The three-way supervision decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperviseDecision {
    Adopt,
    Spawn,
    Replace,
}

impl SuperviseDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            SuperviseDecision::Adopt => "adopt",
            SuperviseDecision::Spawn => "spawn",
            SuperviseDecision::Replace => "replace",
        }
    }
}

/// The gathered health facts + the decision.
#[derive(Debug, Clone)]
pub struct DaemonHealth {
    pub decision: SuperviseDecision,
    pub lock: Option<LockMeta>,
    pub owner_alive: bool,
    pub lock_age_ms: Option<i64>,
    pub status_age_ms: Option<i64>,
}

/// Pure decision over already-gathered facts — the unit-testable core. Port of
/// `decideSupervision`.
#[allow(clippy::too_many_arguments)]
pub fn decide_supervision(
    lock: Option<&LockMeta>,
    owner_alive: bool,
    lock_age_ms: Option<i64>,
    status_age_ms: Option<i64>,
    my_daemon_version: Option<&str>,
    status_fresh_ms: i64,
    boot_grace_ms: i64,
) -> SuperviseDecision {
    let Some(lock) = lock else {
        return SuperviseDecision::Spawn;
    };
    if !owner_alive {
        return SuperviseDecision::Spawn;
    }
    // After an upgrade re-stages the bundle, an old-version daemon that survived
    // launchd's group kill would otherwise be adopted forever — replace it.
    if let Some(v) = my_daemon_version {
        if lock.daemon_version != "unknown" && lock.daemon_version != v {
            return SuperviseDecision::Replace;
        }
    }
    if status_age_ms.is_some_and(|s| s <= status_fresh_ms) {
        return SuperviseDecision::Adopt;
    }
    // Live owner, stale/missing heartbeat: a fresh boot hasn't written one yet —
    // give it the grace window. An unparseable lock age counts as NOT young.
    if lock_age_ms.is_some_and(|l| l <= boot_grace_ms) {
        return SuperviseDecision::Adopt;
    }
    SuperviseDecision::Replace
}

/// ms since an ISO-8601 instant relative to `now_ms` (clamped ≥0), or `None` when
/// unparseable (matches the TS `Number.isNaN` guard). Port of `ageMs`.
fn age_ms(iso: &str, now_ms: i64) -> Option<i64> {
    let t = chrono::DateTime::parse_from_rfc3339(iso)
        .ok()?
        .timestamp_millis();
    Some((now_ms - t).max(0))
}

/// Gather the facts from disk (the daemon lock + `last-status.json`'s `written_at`)
/// and decide. `now_ms` + `my_daemon_version` injected. Never throws. Port of
/// `daemonHealth`.
pub fn daemon_health(now_ms: i64, my_daemon_version: Option<&str>) -> DaemonHealth {
    let lock = read_daemon_lock(&daemon_lock_path());
    let owner_alive = lock
        .as_ref()
        .map(|l| is_process_alive(l.pid))
        .unwrap_or(false);
    let lock_age_ms = lock.as_ref().and_then(|l| age_ms(&l.started_at, now_ms));
    let status_age_ms = read_status_written_at().and_then(|w| age_ms(&w, now_ms));

    let decision = decide_supervision(
        lock.as_ref(),
        owner_alive,
        lock_age_ms,
        status_age_ms,
        my_daemon_version,
        STATUS_FRESH_MS,
        BOOT_GRACE_MS,
    );
    DaemonHealth {
        decision,
        lock,
        owner_alive,
        lock_age_ms,
        status_age_ms,
    }
}

/// The `written_at` field from `last-status.json`, if readable.
fn read_status_written_at() -> Option<String> {
    let raw = std::fs::read_to_string(modelstat_ingest::home_path("last-status.json")).ok()?;
    serde_json::from_str::<Value>(&raw)
        .ok()?
        .get("written_at")?
        .as_str()
        .map(String::from)
}

/// The `_daemon-health` JSON body (§5 shape): `{decision, lock, ownerAlive,
/// lockAgeMs, statusAgeMs}`. `lock` serializes to its camelCase form (or null).
pub fn daemon_health_json(health: &DaemonHealth) -> Value {
    json!({
        "decision": health.decision.as_str(),
        "lock": health.lock.as_ref().map(|l| serde_json::to_value(l).unwrap_or(Value::Null)),
        "ownerAlive": health.owner_alive,
        "lockAgeMs": health.lock_age_ms,
        "statusAgeMs": health.status_age_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock(version: &str) -> LockMeta {
        LockMeta {
            pid: 4242,
            started_at: "2026-07-16T10:00:00.000Z".into(),
            daemon_version: version.into(),
            api_url: "https://modelstat.ai".into(),
        }
    }

    #[test]
    fn no_lock_or_dead_owner_spawns() {
        assert_eq!(
            decide_supervision(
                None,
                false,
                None,
                None,
                None,
                STATUS_FRESH_MS,
                BOOT_GRACE_MS
            ),
            SuperviseDecision::Spawn
        );
        let l = lock("9.9.9");
        assert_eq!(
            decide_supervision(
                Some(&l),
                false,
                Some(1000),
                Some(1000),
                None,
                STATUS_FRESH_MS,
                BOOT_GRACE_MS
            ),
            SuperviseDecision::Spawn
        );
    }

    #[test]
    fn live_owner_with_fresh_heartbeat_is_adopted() {
        let l = lock("9.9.9");
        assert_eq!(
            decide_supervision(
                Some(&l),
                true,
                Some(500_000),
                Some(5_000),
                None,
                STATUS_FRESH_MS,
                BOOT_GRACE_MS
            ),
            SuperviseDecision::Adopt
        );
    }

    #[test]
    fn fresh_boot_without_a_heartbeat_yet_is_adopted_within_grace() {
        let l = lock("9.9.9");
        // No status yet, but the lock is young → grace adopt.
        assert_eq!(
            decide_supervision(
                Some(&l),
                true,
                Some(10_000),
                None,
                None,
                STATUS_FRESH_MS,
                BOOT_GRACE_MS
            ),
            SuperviseDecision::Adopt
        );
    }

    #[test]
    fn wedged_owner_stale_heartbeat_past_grace_is_replaced() {
        let l = lock("9.9.9");
        assert_eq!(
            decide_supervision(
                Some(&l),
                true,
                Some(500_000),
                Some(500_000),
                None,
                STATUS_FRESH_MS,
                BOOT_GRACE_MS
            ),
            SuperviseDecision::Replace
        );
    }

    #[test]
    fn old_version_owner_is_replaced_even_when_healthy() {
        let l = lock("9.9.8");
        // Fresh heartbeat, but the probing CLI is a newer version → replace.
        assert_eq!(
            decide_supervision(
                Some(&l),
                true,
                Some(1_000),
                Some(1_000),
                Some("9.9.9"),
                STATUS_FRESH_MS,
                BOOT_GRACE_MS
            ),
            SuperviseDecision::Replace
        );
        // An "unknown" version in the lock does NOT trigger a version-replace.
        let u = lock("unknown");
        assert_eq!(
            decide_supervision(
                Some(&u),
                true,
                Some(1_000),
                Some(1_000),
                Some("9.9.9"),
                STATUS_FRESH_MS,
                BOOT_GRACE_MS
            ),
            SuperviseDecision::Adopt
        );
    }

    #[test]
    fn age_ms_honors_the_injected_now() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-16T10:00:05.000Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(age_ms("2026-07-16T10:00:00.000Z", now), Some(5000));
        // A future timestamp clamps to 0.
        assert_eq!(age_ms("2026-07-16T10:00:10.000Z", now), Some(0));
        assert_eq!(age_ms("not-a-date", now), None);
    }

    #[test]
    fn json_shape_matches_the_tray_contract() {
        let health = DaemonHealth {
            decision: SuperviseDecision::Adopt,
            lock: Some(lock("9.9.9")),
            owner_alive: true,
            lock_age_ms: Some(1234),
            status_age_ms: None,
        };
        let j = daemon_health_json(&health);
        assert_eq!(j["decision"], "adopt");
        assert_eq!(j["ownerAlive"], true);
        assert_eq!(j["lockAgeMs"], 1234);
        assert_eq!(j["statusAgeMs"], Value::Null);
        // Lock serializes camelCase.
        assert_eq!(j["lock"]["daemonVersion"], "9.9.9");
        assert_eq!(j["lock"]["pid"], 4242);
    }
}
