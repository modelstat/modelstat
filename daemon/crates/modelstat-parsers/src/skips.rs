//! What a parser could not model — counted by the source's own word for it.
//!
//! Every parser matches on a per-record type discriminator and every one of them
//! ends in a catch-all. A catch-all that only `continue`s is invisible: the file
//! parses, the scan reports success, and a whole dialect goes unread until
//! somebody notices the numbers are wrong. Cursor's `ai_code_hashes` disappeared
//! that way and sat dead for weeks.
//!
//! Two things come out of a drop here, and they answer different questions:
//!
//!   * the [`SkipLedger`] — an aggregate `kind -> count` tally that rides the
//!     heartbeat, so "unknown kind X appeared N times across the fleet" is
//!     answerable without opening a laptop;
//!   * an [`unknown_record_event`] — one wire event per record the parser has no
//!     arm for at all, so the record is VISIBLE server-side rather than merely
//!     counted.
//!
//! Neither covers every dropped record, and the line between them is the same
//! one in both cases: **a failure to read, not a decision**.
//!
//!   * A record dropped because the parser could not read it goes in the ledger.
//!     That is the true catch-all (a kind with no arm) AND a kind we do model
//!     arriving without the fields it is defined by — rename `uuid` upstream and
//!     every Claude turn drops silently while the type string still says
//!     `assistant`, which is the v17-class failure wearing a familiar label.
//!   * A record dropped because the parser DECIDED it is not a turn stays out of
//!     both. Codex repeats every message as a `response_item` and taking both
//!     would double each one; Claude's `queue-operation` lines are the CLI's own
//!     bookkeeping. Those drops are the design working, they are high-volume, and
//!     counting them would push the drops that mean something out of a capped
//!     tally.
//!
//! Events are narrower still — only the true catch-alls emit one. A record whose
//! kind we model but cannot read is already legible from the ledger, and minting
//! a wire row for every one of them would flood a session with rows that say
//! nothing the tally did not.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock, PoisonError};

use modelstat_wire::RawEvent;

/// One parse's tally of dropped records, keyed by the kind string the record
/// states about itself.
///
/// Keys are copied VERBATIM from the source and never normalised: the tally has
/// to name the upstream's own word, or a reader cannot grep the upstream's own
/// source for it. A record that states no kind is counted under the empty
/// string — which is not a cosmetic case, it is the loudest one available: if an
/// agent renames its discriminator field, EVERY record lands there at once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkipLedger {
    counts: BTreeMap<String, u64>,
}

impl SkipLedger {
    /// Count one dropped record, and warn the first time this process sees the
    /// kind at all.
    pub fn drop_record(&mut self, source_file: &str, kind: &str) {
        let entry = self.counts.entry(kind.to_string()).or_insert(0);
        // Consult the process-wide set only on this file's first sighting: the
        // warn is one-per-kind-per-process, and a multi-hundred-MB transcript
        // must not take a global lock once per dropped line to discover that.
        let first_in_this_parse = *entry == 0;
        *entry += 1;
        if first_in_this_parse {
            warn_new_kind(source_file, kind);
        }
    }

    #[must_use]
    pub fn into_counts(self) -> BTreeMap<String, u64> {
        self.counts
    }
}

/// One warn per kind per process lifetime, naming the file it was seen in.
///
/// Per PROCESS, not per parse: the daemon re-scans every live transcript on a
/// timer, so a per-parse warn would reprint the same line every cycle forever
/// and train its reader to ignore the log.
fn warn_new_kind(source_file: &str, kind: &str) {
    static SEEN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(BTreeSet::new()));
    let mut guard = seen.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.insert(kind.to_string()) {
        modelstat_log::log_warn!(
            "record kind {kind:?} is not modelled by any parser arm — first seen in {source_file}; \
             it is counted and reported, but nothing in it is read"
        );
    }
}

/// Every numeric leaf under `v`, keyed by its dotted path with the source's own
/// field names, verbatim.
///
/// The reader for a counters object whose schema has moved: the parser cannot
/// bucket the numbers any more, but it can still carry them, and carrying them
/// is the difference between a cost that is recoverable later and one that is
/// gone. Fabricating a zero for the counter that vanished is the alternative,
/// and that is the v17 bug exactly.
///
/// NUMBERS ONLY, structurally. This walks a shape nothing has validated, so a
/// string in it could be anything — including content the redactor never saw. A
/// counters object that moved is still a counters object; its numbers are the
/// whole diagnostic value, and leaving the strings behind closes the hole rather
/// than trusting a guess about what is in them.
#[must_use]
pub fn numeric_leaves(v: &serde_json::Value) -> BTreeMap<String, u64> {
    fn walk(v: &serde_json::Value, path: &str, out: &mut BTreeMap<String, u64>) {
        match v {
            serde_json::Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    out.insert(path.to_string(), u);
                }
            }
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    let next = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    walk(child, &next, out);
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    walk(v, "", &mut out);
    out
}

/// The coordinates of a record whose type no parser arm matches.
///
/// Everything here is either stated by the line itself or is a property of the
/// FILE the parser already established (which session this transcript is, which
/// tool wrote it). There is deliberately no content field of any kind: the
/// redaction floor is written against shapes we know, and a shape we do not know
/// could carry a secret in any position — so an unknown record ships the fact
/// that it existed, its position, and not one byte of what it said.
pub struct UnknownRecord<'a> {
    /// The record's own type discriminator, VERBATIM. Becomes the event `kind`.
    /// The wire carries `kind` as a validated string precisely so a value this
    /// build does not know still round-trips (see `modelstat_wire::enums`).
    ///
    /// This is the record's OWN word, unqualified — codex's `task_started`, not
    /// `event_msg/task_started`. The ledger qualifies its keys by envelope so
    /// two envelopes cannot collide in one tally; an event has an `agent` field
    /// doing that job already, and the point of `kind` is that a reader can grep
    /// the upstream's source for exactly this string.
    pub kind: &'a str,
    pub source_event_id: String,
    pub agent: &'a str,
    pub provider: &'a str,
    pub session_id: String,
    pub ts: String,
    pub turn_index: Option<u64>,
    /// The elapsed time the record stated about itself, if any — see
    /// [`crate::util::stated_duration_ms`]. Structural, like `ts` and the ids
    /// above: a number the record wrote about its own timing, not content. It
    /// is the whole reason codex's `task_complete` is worth shipping, and no
    /// arm has to exist per kind for the next record that measures itself.
    pub duration_ms: Option<u64>,
    pub source_file: &'a str,
    pub source_byte_offset: Option<u64>,
}

/// Build the minimal event for an unmodelled record — see [`UnknownRecord`].
#[must_use]
pub fn unknown_record_event(r: UnknownRecord) -> RawEvent {
    RawEvent {
        source_event_id: r.source_event_id,
        ts: r.ts,
        kind: r.kind.to_string(),
        agent: r.agent.to_string(),
        provider: r.provider.to_string(),
        model: None,
        session_id: r.session_id,
        actor_id: None,
        recipient_actor_id: None,
        turn_index: r.turn_index,
        parent_event_id: None,
        cwd: None,
        git: None,
        tokens: None,
        tokens_unmapped: BTreeMap::new(),
        duration_ms: r.duration_ms,
        tool_calls: BTreeMap::new(),
        files_touched: Vec::new(),
        content_excerpt: None,
        content_bytes: None,
        reasoning_excerpt: None,
        reasoning_bytes: None,
        references: None,
        source_file: Some(r.source_file.to_string()),
        source_byte_offset: r.source_byte_offset,
        redactions: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_every_drop_under_the_source_s_own_word() {
        let mut led = SkipLedger::default();
        led.drop_record("/t.jsonl", "queue-operation");
        led.drop_record("/t.jsonl", "queue-operation");
        led.drop_record("/t.jsonl", "brand_new_kind");
        let counts = led.into_counts();
        assert_eq!(counts["queue-operation"], 2);
        assert_eq!(counts["brand_new_kind"], 1);
    }

    /// A record that states no kind is the schema-move case that matters most:
    /// rename the discriminator field and every record arrives kindless at once.
    #[test]
    fn a_record_that_states_no_kind_is_still_counted() {
        let mut led = SkipLedger::default();
        led.drop_record("/t.jsonl", "");
        assert_eq!(led.into_counts()[""], 1);
    }

    #[test]
    fn an_unknown_record_event_carries_no_content() {
        let ev = unknown_record_event(UnknownRecord {
            kind: "some_new_thing",
            source_event_id: "evt_1".into(),
            agent: "codex_cli",
            provider: "openai",
            session_id: "s1".into(),
            ts: "2026-01-01T00:00:00.000Z".into(),
            turn_index: Some(3),
            duration_ms: Some(6556),
            source_file: "/t.jsonl",
            source_byte_offset: Some(42),
        });
        assert_eq!(ev.kind, "some_new_thing");
        assert_eq!(ev.content_excerpt, None);
        assert_eq!(ev.content_bytes, None);
        assert_eq!(ev.tokens, None);
        assert!(ev.tool_calls.is_empty());
        assert!(ev.files_touched.is_empty());
        assert_eq!(
            ev.duration_ms,
            Some(6556),
            "the timing the record stated about itself survives not being modelled"
        );
    }
}
