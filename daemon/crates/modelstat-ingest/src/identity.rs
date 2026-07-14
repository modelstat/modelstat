//! Device identity store — `~/.modelstat/identity.json`. Byte-for-byte port of
//! the TS `apps/daemon/src/identity.ts` (feature §4). Holds the long-lived bearer
//! (`ds_live_…`), so it is written atomically with `0600` perms; a fresh install
//! reuses it to resume an enrollment instead of minting a duplicate device row.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths::{ensure_home, home_path};
use crate::timefmt::now_iso;

/// The canonical identity record. Serializes to the exact camelCase keys, in the
/// exact order, of the TS `DeviceIdentity` (the `identity.json` contract + the
/// standalone MCP package which reads this file). `claimCode`/`claimUrl`/
/// `userEmail`/`defaultOrgId` are nullable — present as `null` when unset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    pub device_uuid: String,
    pub device_id: String,
    pub bearer_token: String,
    pub claim_code: Option<String>,
    pub claim_url: Option<String>,
    pub hostname: String,
    pub created_at: String,
    pub user_email: Option<String>,
    pub default_org_id: Option<String>,
}

/// Tolerant parse target — every field optional so a hand-edited or partial file
/// deserializes; validation (the three required fields) happens in `parse_file`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialIdentity {
    device_uuid: Option<String>,
    device_id: Option<String>,
    bearer_token: Option<String>,
    claim_code: Option<String>,
    claim_url: Option<String>,
    hostname: Option<String>,
    created_at: Option<String>,
    user_email: Option<String>,
    default_org_id: Option<String>,
}

/// `~/.modelstat/identity.json` (honors `MODELSTAT_HOME`; computed per call).
pub fn identity_path() -> PathBuf {
    home_path("identity.json")
}

/// Whether the identity file exists on disk.
pub fn has_identity_file() -> bool {
    identity_path().exists()
}

use crate::machine_key::host_name;

/// Parse the file. Requires `deviceUuid` + `deviceId` + `bearerToken`; anything
/// else defaults (hostname → this host, createdAt → now, the rest → null). A
/// missing/corrupt/incomplete file reads as `None` — treated as "no identity",
/// exactly like the TS `parseFile()`.
fn parse_file() -> Option<DeviceIdentity> {
    let raw = std::fs::read_to_string(identity_path()).ok()?;
    let obj: PartialIdentity = serde_json::from_str(&raw).ok()?;
    let (device_uuid, device_id, bearer_token) =
        match (obj.device_uuid, obj.device_id, obj.bearer_token) {
            (Some(u), Some(i), Some(b)) if !u.is_empty() && !i.is_empty() && !b.is_empty() => {
                (u, i, b)
            }
            _ => return None,
        };
    Some(DeviceIdentity {
        device_uuid,
        device_id,
        bearer_token,
        claim_code: obj.claim_code,
        claim_url: obj.claim_url,
        hostname: obj.hostname.unwrap_or_else(host_name),
        created_at: obj.created_at.unwrap_or_else(now_iso),
        user_email: obj.user_email,
        default_org_id: obj.default_org_id,
    })
}

/// Read the canonical identity, or `None` if absent/invalid.
pub fn load_identity() -> Option<DeviceIdentity> {
    parse_file()
}

/// Atomic write + `0600` (Unix). Writes `<file>.<pid>.tmp`, then renames — the
/// file is never observed half-populated. Caller decides whether to
/// `backup_identity()` first. Port of the TS `writeAtomic`.
pub fn save_identity(meta: &DeviceIdentity) -> std::io::Result<()> {
    ensure_home()?;
    let path = identity_path();
    let tmp = crate::atomic::with_pid_tmp(&path);
    let json = serde_json::to_string_pretty(meta).expect("DeviceIdentity always serializes");
    std::fs::write(&tmp, json)?;
    crate::atomic::set_file_0600(&tmp);
    std::fs::rename(&tmp, &path)?;
    // rename preserves the tmp mode; re-assert in case umask altered it.
    crate::atomic::set_file_0600(&path);
    Ok(())
}

/// Rename the current identity file to `identity.json.bak-<ISO ts, ':'/'.'→'-'>`.
/// Returns the backup path, or `None` if there was nothing to back up. Port of
/// the TS `backupIdentity`.
pub fn backup_identity() -> Option<PathBuf> {
    let path = identity_path();
    if !path.exists() {
        return None;
    }
    let stamp = now_iso().replace([':', '.'], "-");
    let dest = crate::atomic::sibling(&path, &format!("identity.json.bak-{stamp}"));
    std::fs::rename(&path, &dest).ok()?;
    Some(dest)
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

    fn sample() -> DeviceIdentity {
        DeviceIdentity {
            device_uuid: "6f9619ff-8b86-5d11-b42d-00cf4fc964ff".into(),
            device_id: "device-server-uuid-abc".into(),
            bearer_token: "ds_live_examplefake0000000000000000000000000000".into(),
            claim_code: Some("brave-otter-lake".into()),
            claim_url: Some("https://modelstat.ai/claim/brave-otter-lake".into()),
            hostname: "marks-mac.local".into(),
            created_at: "2026-06-01T10:00:00.000Z".into(),
            user_email: None,
            default_org_id: None,
        }
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            save_identity(&sample()).unwrap();
            assert!(has_identity_file());
            assert_eq!(load_identity().unwrap(), sample());
        });
    }

    #[test]
    fn serialization_matches_golden_shape() {
        // Semantic equality against the committed file-format golden (order- and
        // whitespace-independent) + a key-order spot check.
        let golden_path = format!(
            "{}/../modelstat-wire/tests/golden/file_formats/identity.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let golden: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&golden_path).unwrap()).unwrap();
        let mine: serde_json::Value = serde_json::to_value(sample()).unwrap();
        assert_eq!(
            mine, golden,
            "identity.json shape drifted from the TS golden"
        );

        let s = serde_json::to_string_pretty(&sample()).unwrap();
        let du = s.find("deviceUuid").unwrap();
        let di = s.find("deviceId").unwrap();
        let bt = s.find("bearerToken").unwrap();
        assert!(
            du < di && di < bt,
            "key order must match the TS object literal"
        );
    }

    #[test]
    fn missing_required_field_reads_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            crate::paths::ensure_home().unwrap();
            std::fs::write(identity_path(), r#"{"deviceUuid":"u","deviceId":"d"}"#).unwrap();
            assert!(
                load_identity().is_none(),
                "no bearerToken ⇒ treated as absent"
            );
        });
    }

    #[test]
    fn backup_renames_with_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            save_identity(&sample()).unwrap();
            let bak = backup_identity().expect("a file existed");
            assert!(!has_identity_file(), "original moved aside");
            let name = bak.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("identity.json.bak-"));
            // The timestamp segment after `bak-` has had every ':' and '.'
            // replaced with '-' (feature §4 backup naming).
            let stamp = name.strip_prefix("identity.json.bak-").unwrap();
            assert!(!stamp.contains(':') && !stamp.contains('.'));
        });
    }
}
