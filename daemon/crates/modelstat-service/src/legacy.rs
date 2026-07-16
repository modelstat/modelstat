//! macOS legacy-launcher cleanup on install (§16). The rewrite runs the binary
//! directly; the old node-based launcher's artifacts must be removed so a stale
//! `libnode.*.dylib` can't get loaded and the retired "modelstat agent"
//! renamed-node launcher can't shadow the real binary. Best-effort throughout.

use std::path::Path;

use modelstat_ingest::home_path;

/// Remove the retired node launcher artifacts under `bin`: `modelstat.mjs`, a
/// sibling `node_modules`, the "modelstat agent" renamed-node launcher, and any
/// `libnode.*.dylib` beside them. Pure core (dir injected) so it's testable.
pub fn cleanup_legacy_launcher_in(bin: &Path) {
    let _ = std::fs::remove_file(bin.join("modelstat.mjs"));
    let _ = std::fs::remove_dir_all(bin.join("node_modules"));
    let _ = std::fs::remove_file(bin.join("modelstat agent"));
    if let Ok(rd) = std::fs::read_dir(bin) {
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("libnode.") && name.ends_with(".dylib") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Clean the real install bin dir (`~/.modelstat/bin`).
pub fn cleanup_legacy_launcher() {
    cleanup_legacy_launcher_in(&home_path("bin"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_all_legacy_node_artifacts_leaves_the_binary() {
        let dir = std::env::temp_dir().join(format!("modelstat-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("node_modules/foo")).unwrap();
        std::fs::write(dir.join("modelstat.mjs"), "x").unwrap();
        std::fs::write(dir.join("modelstat agent"), "x").unwrap();
        std::fs::write(dir.join("libnode.127.dylib"), "x").unwrap();
        std::fs::write(dir.join("modelstat"), "the real binary").unwrap();

        cleanup_legacy_launcher_in(&dir);

        assert!(!dir.join("modelstat.mjs").exists());
        assert!(!dir.join("node_modules").exists());
        assert!(!dir.join("modelstat agent").exists());
        assert!(!dir.join("libnode.127.dylib").exists());
        // The real binary is untouched.
        assert!(dir.join("modelstat").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
