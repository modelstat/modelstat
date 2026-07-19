//! Standalone runner for the fake device-API server (see `tests/common/mod.rs`)
//! so the shell e2e (`scripts/e2e-m1.sh`) can drive the REAL `modelstat` binary
//! against it without Docker / the core stack.
//!
//! Bind address: argv[1], else `$MODELSTAT_FAKE_BIND`, else `127.0.0.1:47591`.
//!
//!   cargo run -p modelstat-ingest --example fake_device_server -- 127.0.0.1:47591
//!
//! It shares the exact server code the integration test uses, so the two can't
//! drift.

#[path = "../tests/common/mod.rs"]
mod common;

#[tokio::main]
async fn main() {
    let bind = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("MODELSTAT_FAKE_BIND").ok())
        .unwrap_or_else(|| "127.0.0.1:47591".to_string());
    if let Err(e) = common::serve_on(&bind).await {
        eprintln!("fake-device-server failed to bind {bind}: {e}");
        std::process::exit(1);
    }
}
