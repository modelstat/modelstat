//! M1 acceptance criteria (plan §5 M1), driven end-to-end through the REAL
//! client code (`Config` + `DeviceApi`) against the in-process fake device-API
//! server. Proves, against a live HTTP server, the five scenarios the milestone
//! is "done when":
//!
//!   1. fresh register            — a new device row, unclaimed, bearer persisted
//!   2. credential reuse          — re-register reuses the row (`re_registered`)
//!   3. `--fresh` convergence     — backup + wipe + re-derive → SAME device row
//!   4. 401-revocation recovery   — revoked bearer → recover → fresh bearer, same row
//!   5. prod-guard exit 2         — covered by the CLI test `tests/prod_guard.rs`
//!
//! Each `mk()` builds a fresh `Config`/`DeviceApi` off disk, mimicking separate
//! CLI invocations. One test function (no intra-binary env races); the machine
//! key is process-stable so every scenario derives the same `machine_id`.

mod common;

use std::sync::Arc;

use modelstat_ingest::{
    backup_identity, build_fingerprint, has_identity_file, intended_device_uuid, Config, DeviceApi,
    FreshIdentity,
};
use serde_json::json;

const VERSION: &str = "daemon-0.0.0";

fn mk() -> Arc<DeviceApi> {
    Arc::new(DeviceApi::new(Arc::new(Config::load(VERSION))))
}

fn fresh_from(r: &modelstat_ingest::SelfRegisterResponse) -> FreshIdentity {
    FreshIdentity {
        device_uuid: r.device_uuid.clone(),
        device_id: r.device_id.clone(),
        bearer_token: r.device_secret.clone(),
        claim_code: r.claim_code.clone(),
        claim_url: r.claim_url.clone(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_m1_device_lifecycle() {
    let server = common::spawn().await;
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("MODELSTAT_HOME", home.path());
    std::env::set_var("DAEMON_API_URL", &server.base_url);
    std::env::remove_var("MODELSTAT_SUMMARIZER_MODE");
    std::env::remove_var("MODELSTAT_DEVICE_SALT");

    // ── 1. Fresh register ───────────────────────────────────────────────
    let api = mk();
    assert!(api.config().bearer().is_none(), "fresh install is unpaired");
    let uuid = intended_device_uuid();
    let r1 = api
        .self_register(uuid.clone(), build_fingerprint(VERSION))
        .await
        .expect("fresh register");
    assert_eq!(r1.status, "unclaimed");
    assert_eq!(
        r1.re_registered,
        Some(false),
        "first register is not a re-register"
    );
    assert!(r1.device_secret.starts_with("ds_live_"));
    api.config().save_fresh_identity(fresh_from(&r1)).unwrap();
    let device_id = r1.device_id.clone();
    let bearer1 = r1.device_secret.clone();
    assert!(has_identity_file(), "identity.json written");
    assert_eq!(api.config().bearer().as_deref(), Some(bearer1.as_str()));
    assert_eq!(
        api.config().device_id().as_deref(),
        Some(device_id.as_str())
    );

    // ── 2. Credential reuse ─────────────────────────────────────────────
    // A second invocation loads the identity and re-registers with the STORED
    // uuid; the server dedupes on machine_id back onto the same row.
    let api2 = mk();
    let stored_uuid = api2.config().device_uuid().expect("identity present");
    assert_eq!(stored_uuid, uuid);
    let r2 = api2
        .self_register(stored_uuid, build_fingerprint(VERSION))
        .await
        .expect("re-register");
    assert_eq!(
        r2.device_id, device_id,
        "reuse converges onto the same device row"
    );
    assert_eq!(r2.re_registered, Some(true));

    // ── 3. `--fresh` convergence ────────────────────────────────────────
    // Back up + remove identity.json (what `connect --fresh` does, M6), then a
    // fresh invocation re-derives the SAME machine-stable uuid → same device row.
    let bak = backup_identity().expect("identity existed to back up");
    assert!(!has_identity_file(), "identity moved aside");
    assert!(bak.exists(), "backup written");
    let api3 = mk();
    assert!(
        api3.config().device_uuid().is_none(),
        "no identity after wipe"
    );
    let uuid3 = intended_device_uuid();
    assert_eq!(uuid3, uuid, "same machine re-derives the same uuid");
    let r3 = api3
        .self_register(uuid3, build_fingerprint(VERSION))
        .await
        .expect("fresh re-register");
    assert_eq!(
        r3.device_id, device_id,
        "`--fresh` converges onto the same device row (feature §21.9)"
    );
    assert_eq!(r3.re_registered, Some(true));
    api3.config().save_fresh_identity(fresh_from(&r3)).unwrap();

    // ── 4. 401-revocation recovery ──────────────────────────────────────
    let api4 = mk();
    let bearer_before = api4.config().bearer().expect("paired");
    // Revoke the current bearer server-side (row survives so re-register converges).
    reqwest::Client::new()
        .post(format!("{}/_control/revoke", server.base_url))
        .json(&json!({ "device_id": device_id }))
        .send()
        .await
        .unwrap();
    // A heartbeat now: the dead bearer 401s → deviceRequest recovers by
    // machine-stable re-register → retries with the fresh bearer → succeeds.
    let hb = api4
        .post_heartbeat(
            &device_id,
            &json!({
                "device_id": device_id,
                "status": "watching",
                "daemon_version": VERSION,
            }),
        )
        .await;
    assert!(hb.is_some(), "heartbeat recovered after a 401 revocation");
    let bearer_after = api4.config().bearer().expect("re-paired");
    assert_ne!(
        bearer_after, bearer_before,
        "recovery minted a fresh bearer"
    );
    assert_eq!(
        api4.config().device_id().as_deref(),
        Some(device_id.as_str()),
        "recovery lands on the SAME device row, never a duplicate"
    );

    std::env::remove_var("MODELSTAT_HOME");
    std::env::remove_var("DAEMON_API_URL");
    drop(server);
}
