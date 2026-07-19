//! Shared atomic-write helpers for the identity + state stores (feature §19):
//! write `<file>.<pid>.tmp`, `0600` it, then rename over the target so the file
//! is never observed half-written. On Windows the chmod is a no-op (owner-ACL
//! hardening lands with the service work in M5; the atomic rename still holds).

use std::path::{Path, PathBuf};

/// `<file>.<pid>.tmp`, alongside the target file.
pub(crate) fn with_pid_tmp(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    sibling(path, &format!("{name}.{}.tmp", std::process::id()))
}

/// A sibling path with a new file name, in the same directory as `path`.
pub(crate) fn sibling(path: &Path, name: &str) -> PathBuf {
    match path.parent() {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    }
}

#[cfg(unix)]
pub(crate) fn set_file_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub(crate) fn set_file_0600(_path: &Path) {}
