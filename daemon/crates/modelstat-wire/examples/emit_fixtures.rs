//! Emit Rust's serialization of the wire fixtures for the TS parity test.
//!
//! Reads each `tests/golden/wire/<name>.json` (TS-produced), deserializes it
//! into the Rust struct, and re-serializes it into
//! `tests/golden/wire/rust-emitted/<name>.json`. Because these ARE Rust's
//! serde output of the round-tripped data, having the TS Zod schemas parse them
//! (`packages/core/src/wire-parity.test.ts`) proves TS accepts Rust wire — the
//! second direction of D16.
//!
//! Committed to git; CI re-runs this and gates on `git diff --exit-code` so the
//! Rust serialization can't silently drift from what's checked in.
//!
//!   cargo run -p modelstat-wire --example emit_fixtures

use modelstat_wire::{HeartbeatPayload, IngestBatch, RawEvent, Segment, ToolCallWire};
use serde::{de::DeserializeOwned, Serialize};
use std::path::PathBuf;

fn wire_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/wire")
}

fn emit<T: DeserializeOwned + Serialize>(name: &str) {
    let src = wire_dir().join(name);
    let text = std::fs::read_to_string(&src).unwrap_or_else(|e| panic!("read {src:?}: {e}"));
    let value: T =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("deserialize {name}: {e}"));
    let out_dir = wire_dir().join("rust-emitted");
    std::fs::create_dir_all(&out_dir).unwrap();
    let pretty = serde_json::to_string_pretty(&value).unwrap();
    std::fs::write(out_dir.join(name), format!("{pretty}\n")).unwrap();
    println!("  ✓ rust-emitted/{name}");
}

fn main() {
    emit::<RawEvent>("raw_event_full.json");
    emit::<RawEvent>("raw_event_minimal.json");
    emit::<RawEvent>("raw_event_sdk_instants.json");
    emit::<ToolCallWire>("tool_call.json");
    emit::<Segment>("segment.json");
    emit::<Segment>("segment_with_embedding.json");
    emit::<IngestBatch>("ingest_batch.json");
    emit::<HeartbeatPayload>("heartbeat.json");
    println!("✓ rust-emitted wire fixtures written");
}
