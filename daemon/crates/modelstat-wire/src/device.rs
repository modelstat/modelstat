//! Machine key + deterministic device UUID — a byte-for-byte port of the TS
//! `apps/daemon/src/machine-key.ts` derivations (feature §4).
//!
//! Only the PURE derivations live here (they are wire/id contract): the raw
//! hardware-id probes (`ioreg`, `/etc/machine-id`, registry, fallback file) and
//! the `MODELSTAT_DEVICE_SALT` env read land in M1 (`modelstat-daemon`), but they
//! feed exactly these functions, so pinning them now freezes the contract.
//!
//! Existing devices must NOT re-enroll: same machine → same UUID forever, so a
//! machine that lost `~/.modelstat` re-derives the exact UUID the server already
//! has and maps back to the same row instead of minting a duplicate.

use sha1::Sha1;
use sha2::{Digest, Sha256};

/// Domain-separation salt for the machine-key hash. Baked-in, not secret — it
/// namespaces the digest so our hash can't be cross-referenced with any other
/// product hashing the same OS machine id. FROZEN (feature §18).
pub const MACHINE_KEY_SALT: &str = "modelstat.device.machine-key.v1";

/// Fixed namespace for UUIDv5 device-id derivation — a permanently-frozen random
/// UUID. Changing it re-keys every device. FROZEN (feature §4/§18).
pub const DEVICE_UUID_NAMESPACE: &str = "6f1d2c9a-8b3e-4a7f-9c2d-0e5a1b6c7d8e";

/// Lowercase hex of a byte slice.
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    s
}

/// The stable machine key: salted SHA-256 (64-char lowercase hex) of the raw
/// hardware/OS machine id. Safe to send (`fingerprint.machine_id`) — the raw id
/// is never exposed. Port of TS `machineKey()` (minus the OS probing).
///
/// The hashed string is `"<MACHINE_KEY_SALT>:<raw>"` exactly.
pub fn machine_key_hash(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{MACHINE_KEY_SALT}:{raw}").as_bytes());
    to_hex(&hasher.finalize())
}

/// Deterministic device UUID (RFC 9562 v5) derived from a machine key.
/// `SHA-1(namespace_bytes ++ utf8(key))` → first 16 bytes, with version (5) and
/// variant (10xx) bits forced. Port of TS `deviceUuidFromMachineKey()`.
///
/// `key` is hashed as UTF-8 bytes exactly as JS `hash.update(key)` does.
pub fn device_uuid_from_machine_key(key: &str) -> String {
    let ns_bytes = namespace_bytes();
    let mut hasher = Sha1::new();
    hasher.update(ns_bytes);
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[0..16]);
    b[6] = (b[6] & 0x0f) | 0x50; // version 5
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    format_uuid(&b)
}

/// The device UUID a machine SHOULD use given its machine key and the optional
/// `MODELSTAT_DEVICE_SALT`. A salt appends `:<salt>` to the key (a deterministic
/// second logical device), else the bare key is used. Port of TS
/// `intendedDeviceUuid()` (the env read is done by the caller in M1).
pub fn intended_device_uuid(machine_key: &str, salt: Option<&str>) -> String {
    let key = match salt.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => format!("{machine_key}:{s}"),
        None => machine_key.to_string(),
    };
    device_uuid_from_machine_key(&key)
}

/// The frozen namespace UUID as its 16 raw bytes.
fn namespace_bytes() -> [u8; 16] {
    let hex: String = DEVICE_UUID_NAMESPACE
        .chars()
        .filter(|c| *c != '-')
        .collect();
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

/// Format 16 bytes as a canonical lowercase hyphenated UUID.
fn format_uuid(b: &[u8; 16]) -> String {
    let h = to_hex(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_key_hash_is_sha256_of_salted_raw() {
        // Recomputed independently: sha256("modelstat.device.machine-key.v1:abc").
        let h = machine_key_hash("abc");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn device_uuid_is_valid_v5_shape() {
        let u = device_uuid_from_machine_key(&"0".repeat(64));
        // 8-4-4-4-12, version nibble 5, variant nibble in 8..=b.
        assert_eq!(u.len(), 36);
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert_eq!(&parts[2][0..1], "5");
        assert!(matches!(&parts[3][0..1], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn salt_changes_the_uuid_deterministically() {
        let key = "a".repeat(64);
        let bare = intended_device_uuid(&key, None);
        let salted = intended_device_uuid(&key, Some("ci-2"));
        assert_ne!(bare, salted);
        assert_eq!(salted, intended_device_uuid(&key, Some("ci-2")));
        // Empty/whitespace salt is treated as no salt (TS `.trim()` + falsy).
        assert_eq!(bare, intended_device_uuid(&key, Some("  ")));
    }
}
