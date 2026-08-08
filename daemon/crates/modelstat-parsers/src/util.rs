//! Small shared string helpers ported from the TS parsers' excerpt/name paths,
//! plus a timeout-bounded subprocess runner used by the git + discovery probes.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run `program args` (optionally in `cwd`), returning stdout on a zero exit
/// within `timeout`, else None. Best-effort: any spawn/exit/timeout failure is
/// None (probes must never block or fail a scan). A reader thread drains stdout
/// so the child never blocks on a full pipe; on timeout the child is killed.
pub fn run_command(
    program: &str,
    args: &[&str],
    cwd: Option<&str>,
    timeout: Duration,
) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                break None;
            }
        }
    };
    let out = reader.join().ok()?;
    match status {
        Some(s) if s.success() => Some(out),
        _ => None,
    }
}

/// A duration the record STATED about itself, in milliseconds — or None when it
/// stated none.
///
/// Never derived, only passed through: the elapsed time an agent measured is a
/// fact only that agent holds, and a number this parser computed from two
/// timestamps would be a different quantity wearing the same name.
///
/// Agents write it under their own name and their own unit, and the set of
/// spellings is open. Claude Code alone writes three across three tools of ONE
/// release — `durationMs` (web fetch), `durationSeconds` (web search),
/// `totalDurationMs` (the sub-agent tool) — and codex writes `duration_ms` on
/// `task_complete`. A fixed list of today's spellings drops the next one on the
/// day it ships, and the number is unrecoverable afterwards: only the device
/// ever held the log.
///
/// So the probe reads the SHAPE the field states about itself — a key that
/// names a duration, ending in the unit it is counted in. Two consequences are
/// deliberate: `timeoutMs` and `retryInMs` are not durations and are not read
/// (a configured limit is not an elapsed time), and the record's OWN fields are
/// read one level deep, never recursively — a nested object is a shape nothing
/// here has validated, and a number inside it says nothing about this record.
///
/// More than one stated duration is not an observed case. The first in the
/// record's own key order wins, so a replayed scan reads the same number.
#[must_use]
pub fn stated_duration_ms(record: &serde_json::Value) -> Option<u64> {
    record.as_object()?.iter().find_map(|(key, value)| {
        let key = key.to_ascii_lowercase();
        if !key.contains("duration") {
            return None;
        }
        let per_unit_ms = if key.ends_with("ms") {
            1.0
        } else if key.ends_with("sec") || key.ends_with("secs") || key.ends_with("seconds") {
            1000.0
        } else {
            return None;
        };
        let stated = value.as_f64()?;
        if !stated.is_finite() || stated < 0.0 {
            return None;
        }
        Some((stated * per_unit_ms).round() as u64)
    })
}

/// Take at most `max` UTF-16 code units of `s`, matching JS `String.slice(0,max)`
/// (which slices UTF-16 units, not code points). BMP-only inputs slice identically
/// to a char count.
pub fn slice_utf16(s: &str, max: usize) -> String {
    if s.encode_utf16().count() <= max {
        return s.to_string();
    }
    let units: Vec<u16> = s.encode_utf16().take(max).collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::stated_duration_ms;
    use serde_json::json;

    /// The four spellings real agents write today, each read in the unit it
    /// states. Shapes only — no transcript content is copied here.
    #[test]
    fn every_spelling_an_agent_states_is_read_in_its_own_unit() {
        // Claude Code, three tools of one release.
        assert_eq!(
            stated_duration_ms(&json!({"url": "https://example.test", "durationMs": 3500})),
            Some(3500)
        );
        assert_eq!(
            stated_duration_ms(&json!({"searchCount": 2, "durationSeconds": 7.25})),
            Some(7250),
            "seconds are converted, fractions rounded"
        );
        assert_eq!(
            stated_duration_ms(&json!({"agentType": "general", "totalDurationMs": 105_000})),
            Some(105_000)
        );
        // codex, on `task_complete`.
        assert_eq!(
            stated_duration_ms(&json!({"duration_ms": 6556, "time_to_first_token_ms": 3076})),
            Some(6556),
            "the token-latency field is not a duration of this record"
        );
    }

    /// A configured limit is not an elapsed time. Both of these sit on real
    /// records beside the durations above.
    #[test]
    fn a_limit_or_a_delay_is_never_mistaken_for_a_duration() {
        assert_eq!(stated_duration_ms(&json!({"timeoutMs": 120_000})), None);
        assert_eq!(stated_duration_ms(&json!({"retryInMs": 4000})), None);
        assert_eq!(stated_duration_ms(&json!({"totalTokens": 900})), None);
    }

    #[test]
    fn a_record_that_states_no_duration_states_none() {
        assert_eq!(stated_duration_ms(&json!({})), None);
        assert_eq!(stated_duration_ms(&json!("a string")), None);
        assert_eq!(
            stated_duration_ms(&json!({"durationMs": "3500"})),
            None,
            "a duration that stopped being a number is not read as one"
        );
        assert_eq!(
            stated_duration_ms(&json!({"durationMs": -1})),
            None,
            "negative elapsed time is not a reading"
        );
        assert_eq!(
            stated_duration_ms(&json!({"duration": 12})),
            None,
            "a duration in no stated unit cannot be converted"
        );
        assert_eq!(
            stated_duration_ms(&json!({"durationMs": {"total": 5}})),
            None,
            "one level deep: a nested shape is not walked"
        );
    }

    /// The spelling that has not shipped yet is the one this exists for.
    #[test]
    fn an_unseen_spelling_is_read_because_it_states_its_own_unit() {
        assert_eq!(
            stated_duration_ms(&json!({"elapsedDurationSeconds": 2})),
            Some(2000)
        );
        assert_eq!(
            stated_duration_ms(&json!({"tool_duration_ms": 40})),
            Some(40)
        );
    }
}
