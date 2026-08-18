//! Stable per-machine key + fingerprint — the impure half (feature §4). The
//! PURE derivations
//! (`machine_key_hash`, `device_uuid_from_machine_key`, `intended_device_uuid`)
//! already live in `modelstat-wire`; this module adds the OS/hardware probes,
//! the persisted fallback key, the env reads, and `build_fingerprint`.
//!
//! The anchor is an OS/hardware id that lives OUTSIDE anything we install, so the
//! SAME physical machine derives the SAME device identity across reinstalls and
//! even after `~/.modelstat` is wiped — the property that stops one Mac becoming
//! three dashboard rows (feature §21.9). The raw id never leaves the machine:
//! `machine_key()` returns a salted SHA-256 of it (via `modelstat-wire`).

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use modelstat_wire::{machine_key_hash, MACHINE_KEY_SALT};
use sha2::{Digest, Sha256};

use crate::paths::{ensure_home, home_path};

/// Which source produced the raw machine id. Surfaced by `paths`/diagnostics so
/// a user can tell a hardware anchor from the weaker file fallback. Values are
/// the exact strings the TS `MachineKeySource` union uses (the `paths --json`
/// contract, feature §5).
pub type MachineKeySource = &'static str;

/// Memoised for the process, exactly like the TS module-level `cachedKey` /
/// `cachedSource`. The key is genuinely process-stable (a hardware id or a
/// persisted file), so caching it can't drift within a run.
static CACHE: OnceLock<(String, MachineKeySource)> = OnceLock::new();

/// Lowercase hex of a byte slice.
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

/// Run a command capturing stdout, killing it if it outruns `timeout` (the TS
/// `spawnSync(..., { timeout })`). Returns trimmed stdout on a clean `0` exit,
/// else `None`. The probes we run (`ioreg`, `reg query`) emit a few KB — well
/// under the OS pipe buffer — so reading after exit can't deadlock.
fn spawn_capture(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// macOS: `IOPlatformUUID` out of `ioreg` (4s timeout).
#[cfg(target_os = "macos")]
fn macos_platform_uuid() -> Option<String> {
    let out = spawn_capture(
        "ioreg",
        &["-rd1", "-c", "IOPlatformExpertDevice"],
        Duration::from_secs(4),
    )?;
    let re = regex::Regex::new(r#""IOPlatformUUID"\s*=\s*"([^"]+)""#).ok()?;
    let v = re.captures(&out)?.get(1)?.as_str().trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Linux: systemd `machine-id`, then the dbus fallback path.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn linux_machine_id() -> Option<String> {
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(v) = std::fs::read_to_string(p) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Windows: `MachineGuid` from the registry (4s timeout).
#[cfg(target_os = "windows")]
fn windows_machine_guid() -> Option<String> {
    let out = spawn_capture(
        "reg",
        &[
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ],
        Duration::from_secs(4),
    )?;
    let re = regex::Regex::new(r"MachineGuid\s+REG_SZ\s+([0-9a-fA-F-]+)").ok()?;
    let v = re.captures(&out)?.get(1)?.as_str().trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// The persisted random fallback key file.
///
/// **§23 fix:** honors `MODELSTAT_HOME`. The TS hard-coded `~/.modelstat/
/// machine-key` (ignoring the override); the rewrite reads `home_path(...)` so a
/// relocated install keeps its fallback key with the rest of its state. A
/// fallback-key device that *relocated* may re-derive (documented, acceptable);
/// hardware-id devices — the overwhelming majority — are unaffected.
fn fallback_key_file() -> PathBuf {
    home_path("machine-key")
}

/// Read (or lazily create) the persisted random fallback key. The last resort
/// when no hardware id is readable (containers, locked-down envs); survives
/// reinstalls but NOT a home wipe, so it's strictly worse than a hardware id.
fn fallback_key() -> String {
    let file = fallback_key_file();
    if let Ok(v) = std::fs::read_to_string(&file) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    let fresh = generate_fallback_key();
    // Best effort — even an unpersisted key is stable for this process.
    if ensure_home().is_ok() {
        let _ = write_owner_only(&file, &fresh);
    }
    fresh
}

/// 64-hex of high-entropy process-local sources. The TS uses
/// `sha256(salt:fallback:Date.now():Math.random())`; this mixes the wall clock,
/// pid, a randomly-seeded hasher, and a stack address — the exact bytes are not a
/// contract (the key is random per install and then persisted).
fn generate_fallback_key() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(nanos);
    h.write_u32(pid);
    let seed = h.finish();
    let stack = &pid as *const u32 as usize;
    let mut hasher = Sha256::new();
    hasher.update(format!("{MACHINE_KEY_SALT}:fallback:{nanos}:{pid}:{seed}:{stack}").as_bytes());
    to_hex(&hasher.finalize())
}

/// Write a small state file with owner-only perms (`0600` on Unix), creating it
/// fresh. Used for the fallback key; the identity/state stores have their own
/// atomic writer.
fn write_owner_only(path: &PathBuf, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Resolve the raw machine id + which source produced it (per-OS probe order,
/// then the file fallback).
fn resolve_raw() -> (String, MachineKeySource) {
    #[cfg(target_os = "macos")]
    {
        if let Some(v) = macos_platform_uuid() {
            return (v, "macos-ioplatform");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(v) = windows_machine_guid() {
            return (v, "windows-guid");
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(v) = linux_machine_id() {
            return (v, "linux-machine-id");
        }
    }
    (fallback_key(), "fallback-file")
}

/// The stable machine key: salted SHA-256 (64-char hex) of the raw hardware/OS
/// machine id. Memoised for the process. Safe to send (`fingerprint.machine_id`)
/// — the raw id is never exposed. Port of TS `machineKey()`.
pub fn machine_key() -> String {
    CACHE.get_or_init(|| {
        let (raw, source) = resolve_raw();
        (machine_key_hash(&raw), source)
    });
    CACHE.get().unwrap().0.clone()
}

/// Which source the (memoised) key came from. Port of TS `machineKeySource()`.
pub fn machine_key_source() -> MachineKeySource {
    machine_key();
    CACHE.get().unwrap().1
}

/// The `MODELSTAT_DEVICE_SALT` value, trimmed and non-empty, else `None`.
fn device_salt() -> Option<String> {
    std::env::var("MODELSTAT_DEVICE_SALT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The device UUID this machine SHOULD use. `MODELSTAT_DEVICE_SALT` appends
/// `:<salt>` to the key for a deterministic SECOND logical device (CI matrices,
/// multi-tenant boxes). Port of TS `intendedDeviceUuid()`.
pub fn intended_device_uuid() -> String {
    modelstat_wire::intended_device_uuid(&machine_key(), device_salt().as_deref())
}

/// `os_family` — feature §4 gains `windows` (the enum already exists server-side,
/// `OS_FAMILIES` in `modelstat-wire`); the TS returned `"other"` for Windows.
fn os_family() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        _ => "other",
    }
}

/// `arch` ∈ x86_64 | arm64 | other (feature §4). Node reports `x64`/`arm64`;
/// Rust reports `x86_64`/`aarch64` — mapped to the same wire values.
fn os_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "arm64",
        _ => "other",
    }
}

/// `os_version` = Node `os.release()` (the kernel/OS release string). Unix reads
/// `uname -r` (byte-identical to Node on macOS/Linux); Windows best-effort parses
/// `cmd /c ver` (not golden-tested; Windows service specifics land in M5).
fn os_version() -> String {
    #[cfg(not(windows))]
    {
        spawn_capture("uname", &["-r"], Duration::from_secs(4))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(windows)]
    {
        spawn_capture("cmd", &["/c", "ver"], Duration::from_secs(4))
            .and_then(|s| {
                regex::Regex::new(r"(\d+\.\d+\.\d+)").ok().and_then(|re| {
                    re.captures(&s)
                        .and_then(|c| c.get(1))
                        .map(|m| m.as_str().to_string())
                })
            })
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// The machine hostname (Node `os.hostname()`). Shared by the fingerprint, the
/// identity store's write-time hostname, and `Config`.
pub(crate) fn host_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The single device fingerprint, shared by register (`POST /v1/tokens`) and
/// heartbeat. `machine_id` is the server's dedupe anchor — byte-identical on both
/// paths because both read `machine_key()`. Port of TS `buildFingerprint()`.
pub fn build_fingerprint(daemon_version: &str) -> modelstat_wire::Fingerprint {
    modelstat_wire::Fingerprint {
        hostname: host_name(),
        os_family: os_family().to_string(),
        os_version: os_version(),
        arch: os_arch().to_string(),
        daemon: "modelstat-daemon".to_string(),
        daemon_version: daemon_version.to_string(),
        machine_id: machine_key(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env_lock;

    #[test]
    fn fallback_key_file_honors_modelstat_home() {
        // The §23 fix: the fallback key path tracks MODELSTAT_HOME (the TS
        // hard-coded ~/.modelstat and ignored the override).
        let _g = test_env_lock();
        std::env::set_var("MODELSTAT_HOME", "/tmp/ms-home-xyz");
        assert_eq!(
            fallback_key_file(),
            PathBuf::from("/tmp/ms-home-xyz/machine-key")
        );
        std::env::remove_var("MODELSTAT_HOME");
    }

    #[test]
    fn machine_key_is_64_hex_and_stable() {
        let a = machine_key();
        let b = machine_key();
        assert_eq!(a, b, "memoised — stable within the process");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn intended_uuid_is_v5_shape() {
        let u = intended_device_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "5", "version nibble 5");
    }

    #[test]
    fn fingerprint_fields_are_wellformed() {
        let fp = build_fingerprint("daemon-0.0.0");
        assert_eq!(fp.daemon, "modelstat-daemon");
        assert_eq!(fp.daemon_version, "daemon-0.0.0");
        assert!(["macos", "linux", "windows", "other"].contains(&fp.os_family.as_str()));
        assert!(["x86_64", "arm64", "other"].contains(&fp.arch.as_str()));
        assert_eq!(fp.machine_id, machine_key());
        assert!(!fp.hostname.is_empty());
    }

    #[test]
    fn device_salt_trims_and_nulls_blank() {
        let _g = test_env_lock();
        std::env::set_var("MODELSTAT_DEVICE_SALT", "  ci-2 ");
        assert_eq!(device_salt().as_deref(), Some("ci-2"));
        std::env::set_var("MODELSTAT_DEVICE_SALT", "   ");
        assert_eq!(device_salt(), None);
        std::env::remove_var("MODELSTAT_DEVICE_SALT");
    }
}
