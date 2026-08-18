//! Deterministic id derivation — a byte-for-byte port of the TypeScript
//! `@modelstat/core/ids` + `@modelstat/parsers/tool-hash` id helpers.
//!
//! These derivations are a WIRE CONTRACT: the server dedupes on
//! `(scope_id, source_event_id)` and upserts segments by `segment_id`, so any
//! drift here re-ingests all history under fresh ids and double-counts. The
//! golden vectors in `tests/golden/ids.json` (generated from the TS) pin every
//! value; do not change an algorithm without a PROCESSING_VERSION story.
//!
//! ## djb2-64 (the shared primitive)
//!
//! TS computes, per UTF-16 code unit `c` of the key string:
//! ```text
//! h = 5381
//! h = (h * 33) ^ c   (mod 2^64, applied after each step)
//! ```
//! then base36-encodes the unsigned 64-bit result. Two subtleties the port
//! must preserve exactly:
//!   1. **UTF-16 code units, not bytes or code points.** `String.charCodeAt`
//!      yields UTF-16 units, so astral characters contribute their surrogate
//!      halves. We iterate `str::encode_utf16()` to match.
//!   2. **Wrapping arithmetic.** `(h*33) ^ c & MASK` in JS BigInt equals
//!      `h.wrapping_mul(33) ^ (c as u64)` because bitwise-AND distributes over
//!      XOR and `c < 2^16`, so masking after the XOR == masking the product.

/// djb2-64 over the UTF-16 code units of `s`, base36-encoded (lowercase, no
/// leading zeros) — the shared core of every `evt_`/`seg_`/`tc_` id.
fn djb2_base36(s: &str) -> String {
    let mut h: u64 = 5381;
    for unit in s.encode_utf16() {
        h = h.wrapping_mul(33) ^ (unit as u64);
    }
    base36(h)
}

/// Unsigned base36, matching JavaScript `BigInt.prototype.toString(36)`:
/// digits `0-9a-z`, lowercase, and `"0"` for zero (never an empty string).
fn base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = [0u8; 13]; // u64 max is 13 base36 digits
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = DIGITS[(n % 36) as usize];
        n /= 36;
    }
    // Safe: buf holds only ASCII digits.
    String::from_utf8(buf[i..].to_vec()).unwrap()
}

/// The legal `source` shapes for [`source_event_id`], mirroring the TS
/// discriminated union. The key string each produces is the frozen contract.
pub enum EventSource<'a> {
    /// CLI daemon parsing JSONL: `fs::<file>::<byteOffset>`. The legacy 3-arg
    /// `sourceEventId(deviceId, filePath, byteOffset)` form hashes identically.
    File { file: &'a str, byte_offset: u64 },
    /// Chrome-extension web capture: `web::<host>::<conv>::<msg>`.
    Web {
        host: &'a str,
        conversation_id: &'a str,
        message_id: &'a str,
    },
    /// Position-independent key for transcript lines whose identity is the uuid
    /// embedded in the line (Claude Code resume-copy dedupe): `uuid::<lineUuid>`.
    LineUuid { line_uuid: &'a str },
    /// Position-independent key for a codex round trip:
    /// `codex::<conversation>::<input>:<cached>:<output>:<reasoning>`, the
    /// conversation's CUMULATIVE counter at that turn.
    ///
    /// Codex has no per-line uuid, so [`LineUuid`](EventSource::LineUuid) — the
    /// shape that makes Claude Code's resume copies collapse — has nothing to
    /// key on. A fork rollout file replays its ancestor's whole history
    /// verbatim, and codex REWRITES the copied timestamps to the fork moment,
    /// so neither the byte offset nor the timestamp survives a copy. The
    /// cumulative counter does, byte for byte, and it is what upstream itself
    /// treats as the conversation's running total.
    CodexTurn {
        /// The CONVERSATION whose running total this is — the id the rollout
        /// region declares, which inside a replayed prefix is the ANCESTOR's.
        /// Deliberately NOT the session id: a fork rollout is its own session
        /// (its filename says so) while the history it replays still belongs,
        /// counter and all, to the conversation it was copied from. Keying on
        /// the session instead would mint a fresh id per fork file and bill one
        /// conversation once per fork — the bug this shape exists to prevent.
        conversation_id: &'a str,
        /// `total_token_usage`, in the order upstream declares it.
        cumulative: [u64; 4],
    },
}

/// Deterministic dedupe key for a single parsed event: `evt_<base36(djb2-64)>`
/// over `<deviceId>::<shape-key>`. Port of TS `sourceEventId`.
pub fn source_event_id(device_id: &str, source: &EventSource) -> String {
    let key = match source {
        EventSource::File { file, byte_offset } => format!("fs::{file}::{byte_offset}"),
        EventSource::LineUuid { line_uuid } => format!("uuid::{line_uuid}"),
        EventSource::Web {
            host,
            conversation_id,
            message_id,
        } => format!("web::{host}::{conversation_id}::{message_id}"),
        // The key string here is frozen: `codex::…` is what every id already
        // landed under, so naming the field honestly must not move a hash.
        EventSource::CodexTurn {
            conversation_id,
            cumulative: [i, c, o, r],
        } => format!("codex::{conversation_id}::{i}:{c}:{o}:{r}"),
    };
    format!("evt_{}", djb2_base36(&format!("{device_id}::{key}")))
}

/// Deterministic segment id: `seg_<base36(djb2-64)>` over
/// `<sessionId>|<startMs>|<endMs>|<sorted(sourceEventIds) joined by ",">`.
/// Port of TS `segmentId`. Sort is lexicographic over the id strings (matching
/// JS `Array.prototype.sort()` default, which for these ASCII ids is byte order).
pub fn segment_id(
    session_id: &str,
    started_at_ms: i64,
    ended_at_ms: i64,
    source_event_ids: &[String],
) -> String {
    let mut sorted: Vec<&str> = source_event_ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let joined = sorted.join(",");
    let s = format!("{session_id}|{started_at_ms}|{ended_at_ms}|{joined}");
    format!("seg_{}", djb2_base36(&s))
}

/// Deterministic fallback `external_call_id` when a source line carries no id:
/// `tc_<base36(djb2-64)>` of `<sourceEventId>|<callIndex>`. Port of TS
/// `fallbackCallId` (`@modelstat/parsers/tool-hash`); same djb2-64 family.
pub fn tc_fallback_id(source_event_id: &str, call_index: u64) -> String {
    format!(
        "tc_{}",
        djb2_base36(&format!("{source_event_id}|{call_index}"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Inline sanity checks; the authoritative parity assertions live in
    // tests/golden_ids.rs against the TS-generated vectors.
    #[test]
    fn fs_shape_matches_ts_golden() {
        let id = source_event_id(
            "dev_1",
            &EventSource::File {
                file: "/x/a.jsonl",
                byte_offset: 42,
            },
        );
        assert_eq!(id, "evt_16zw770jnvito");
    }

    #[test]
    fn line_uuid_shape_matches_ts_golden() {
        let id = source_event_id(
            "dev_1",
            &EventSource::LineUuid {
                line_uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            },
        );
        assert_eq!(id, "evt_irnlblnsf9gx");
    }

    #[test]
    fn base36_zero_is_zero() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
    }
}
