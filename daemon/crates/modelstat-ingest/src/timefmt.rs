//! ISO-8601 timestamps byte-compatible with JS `new Date().toISOString()`
//! (always millisecond precision, always a `Z` suffix). Used for `createdAt`,
//! the `last-status.json` `written_at`, and the identity backup stamp.

use chrono::{SecondsFormat, Utc};

/// `new Date().toISOString()` — e.g. `2026-07-15T12:34:56.789Z`. chrono's
/// `use_z = true` gives the `Z` (not `+00:00`) and `Millis` fixes the 3-digit
/// fraction, so the shape matches JS byte-for-byte.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Milliseconds since the Unix epoch — the JS `Date.now()` used for the reship /
/// recovery bookkeeping timestamps.
pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}
