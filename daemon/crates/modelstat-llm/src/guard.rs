//! The GPU-abort guard file (feature §10.2): `models/.metal-load-guard`, armed
//! before the first GPU touch and disarmed only after a successful GPU probe.
//! Present (and naming the current engine version) ⇒ CPU-only; it sticks until
//! manually deleted or the engine version changes (no crash↔CPU oscillation).

use std::path::{Path, PathBuf};

pub const GUARD_FILE: &str = ".metal-load-guard";

pub fn guard_path(models_dir: &Path) -> PathBuf {
    models_dir.join(GUARD_FILE)
}

/// Armed (⇒ force CPU) when the guard exists AND names the current version. A
/// version change re-probes the GPU.
pub fn is_armed(models_dir: &Path, version: &str) -> bool {
    match std::fs::read_to_string(guard_path(models_dir)) {
        Ok(content) => content.trim() == version,
        Err(_) => false,
    }
}

/// Arm the guard (stamp it with `version`) before touching the GPU.
pub fn arm(models_dir: &Path, version: &str) -> std::io::Result<()> {
    if let Some(parent) = guard_path(models_dir).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(guard_path(models_dir), version.as_bytes())
}

/// Disarm the guard after a successful GPU probe (idempotent — ok if absent).
pub fn disarm(models_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(guard_path(models_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_disarm_version_gated() {
        let dir = std::env::temp_dir().join(format!("modelstat-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_armed(&dir, "v1"));
        arm(&dir, "v1").unwrap();
        assert!(is_armed(&dir, "v1"));
        // A different engine version re-probes (treated as not-armed).
        assert!(!is_armed(&dir, "v2"));
        disarm(&dir).unwrap();
        assert!(!is_armed(&dir, "v1"));
        disarm(&dir).unwrap(); // idempotent
        let _ = std::fs::remove_dir_all(&dir);
    }
}
