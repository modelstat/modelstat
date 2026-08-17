//! The device's time zone, read off the OS.
//!
//! The one fact about a working day that only the machine holds. Every instant
//! on the wire is UTC, so once a batch leaves the box nothing downstream can
//! recover what time it was for the person doing the work — 09:00 in Berlin and
//! 09:00 in São Paulo arrive as the same two numbers seven hours apart.
//!
//! Two readings, both stated, neither derived from the other:
//!
//!   * the IANA NAME (`Europe/Berlin`) — the durable fact. It survives DST and
//!     it is what a reader needs to render any past instant correctly.
//!   * the CURRENT OFFSET in minutes east of UTC — the reading in force right
//!     now, DST included. It is what a reader needs when it has no zone
//!     database to hand, which is most readers.
//!
//! The name is passed through verbatim and never checked against a roster of
//! zones this build has heard of: the zone database gains and moves entries, and
//! a name we cannot place is still the truthful answer to what this machine is
//! set to. When the OS will not say, we say nothing — no zone is not UTC.

use chrono::{Local, Offset};

/// The device's IANA zone name, or `None` when the OS states none.
///
/// Verbatim, including the spellings that are aliases or links in the zone
/// database (`US/Pacific`, `Asia/Calcutta`): what the machine is SET to is the
/// fact, and canonicalising it here would be this process deciding it knows the
/// zone database better than the machine does.
#[must_use]
pub fn device_timezone() -> Option<String> {
    iana_time_zone::get_timezone()
        .ok()
        .map(|tz| tz.trim().to_string())
        .filter(|tz| !tz.is_empty())
}

/// Minutes east of UTC on this device right now (`-420` for UTC-7), DST
/// included — the offset in force at this instant, not the one the zone's rules
/// read on paper.
#[must_use]
pub fn device_utc_offset_minutes() -> i32 {
    Local::now().offset().fix().local_minus_utc() / 60
}

/// [`device_utc_offset_minutes`] in the width the batch wire states
/// (`tz_offset_minutes: Option<i16>`, matching the server's contract).
///
/// `None` on a reading `i16` cannot hold — unreachable while chrono bounds an
/// offset to ±1440 minutes, but the honest answer if it ever weren't: an
/// unrepresentable reading ships as silence, never as a clipped guess.
#[must_use]
pub fn device_tz_offset_minutes() -> Option<i16> {
    i16::try_from(device_utc_offset_minutes()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe answers on the machine running the tests, and answers within
    /// the range UTC offsets actually occupy. Asserting a VALUE would only
    /// assert what CI's `TZ` happens to be.
    #[test]
    fn the_offset_is_a_real_utc_offset() {
        let mins = device_utc_offset_minutes();
        // The same bound the wire states for every offset it carries.
        assert!(
            (-840..=840).contains(&mins),
            "{mins} is outside the wire's range"
        );
    }

    /// The wire-width reading is the same number as the `i32` probe — the cast
    /// changes representation, never the value.
    #[test]
    fn the_wire_width_offset_is_the_same_reading() {
        assert_eq!(
            device_tz_offset_minutes().map(i32::from),
            Some(device_utc_offset_minutes())
        );
    }

    /// A zone NAME, when the OS gives one, is a non-empty string that fits the
    /// wire's cap. Skipped rather than failed where the OS says nothing — a
    /// container with no zone configured is a real machine, and the daemon's
    /// answer there is to stay silent.
    #[test]
    fn a_stated_zone_name_is_non_empty_and_fits_the_wire() {
        if let Some(tz) = device_timezone() {
            assert!(!tz.is_empty());
            assert!(
                tz.len() <= modelstat_wire::caps::TIMEZONE_MAX,
                "zone name {tz} exceeds the wire cap"
            );
        }
    }
}
