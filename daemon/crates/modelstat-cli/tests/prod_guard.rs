//! AC #5 (feature §4 CI guard, plan §5 M1): a FRESH self-register against the
//! un-overridden prod API from a non-interactive/CI context is refused with
//! **exit 2**. Drives the built `modelstat` binary in a throwaway home so it
//! never touches the real `~/.modelstat` — and, because the guard fires BEFORE
//! any network call, it never contacts production.

use std::process::Command;

/// Path to the freshly-built `modelstat` binary (cargo sets this for the test).
const BIN: &str = env!("CARGO_BIN_EXE_modelstat");

#[test]
fn fresh_prod_register_from_ci_exits_2() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .arg("self-register")
        .env("MODELSTAT_HOME", home.path())
        .env("CI", "1") // non-interactive
        // Ensure prod default: no backend override, no explicit opt-in.
        .env_remove("DAEMON_API_URL")
        .env_remove("MODELSTAT_ALLOW_PROD_REGISTER")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run modelstat self-register");

    assert_eq!(
        out.status.code(),
        Some(2),
        "prod/CI register guard must exit 2 (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    // No identity was created (the guard fired before any register).
    assert!(
        !home.path().join("identity.json").exists(),
        "guard must refuse before writing any identity"
    );
    // The remedies are printed to stderr.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("MODELSTAT_ALLOW_PROD_REGISTER"));
    assert!(stderr.contains("DAEMON_API_URL"));
}

#[test]
fn non_prod_api_bypasses_the_guard() {
    // The guard only protects the un-overridden PROD default. With DAEMON_API_URL
    // pointed at a backend, `is_prod_default_api` is false, so the guard is
    // skipped and the command proceeds to the network call. Point at an
    // unreachable local port so it fails FAST (connection refused, exit 1) —
    // proving the guard was bypassed (exit ≠ 2) without ever touching prod.
    //
    // (The other bypass — MODELSTAT_ALLOW_PROD_REGISTER=1 on the prod default —
    // is intentionally NOT exercised here: it would POST against real production.
    // Its two terms are covered by unit tests for `is_prod_default_api` and the
    // `env_flag`/`prod_register_opt_in` helpers.)
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .arg("self-register")
        .env("MODELSTAT_HOME", home.path())
        .env("CI", "1")
        .env("DAEMON_API_URL", "http://127.0.0.1:1") // refused immediately
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run modelstat self-register");
    assert_ne!(
        out.status.code(),
        Some(2),
        "a non-prod DAEMON_API_URL bypasses the guard"
    );
}
