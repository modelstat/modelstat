//! Local processing-pipeline version — a port of
//! `apps/daemon/src/processing-version.ts`.
//!
//! The markers that let a new daemon build force a re-scan of previously
//! uploaded sessions. File cursors track "uploaded up to byte N", so a normal
//! restart only ships new events — but when the pipeline ITSELF changes shape
//! (capture, redaction, a parser's schema handling), the affected output is
//! stale even though the JSONL hasn't moved. On startup the daemon compares
//! the compiled-in PER-ASPECT versions ([`ASPECT_VERSIONS`]) to the stored
//! ones; each stale aspect wipes exactly the cursors it invalidates — a
//! parser-scoped fix re-reads one parser's files, a capture/redaction change
//! re-reads the world (a re-scan REPLACES segments/messages by id in place —
//! no duplicates, no orphans). The single-integer v1–v23 history below is the
//! era when every bump claimed the world; see [`LEGACY_WORLD_VERSION`].

use modelstat_ingest::RuntimeState;

/// Current local processing-pipeline version. Bump when the pipeline produces
/// materially different output for the same input.
///
/// v16 — the Rust rewrite's cutover value, absorbing the runtime/model swaps the
///       TS never had (the candle BGE embedder + BERT-NER, and the prompt-fed
///       non-determinism of a different engine) in one bump, so every historical
///       session re-scanned once at cutover. (The TS chain ended at v15; see that
///       file for the v1–v15 history.)
/// v17 — codex token accounting fix. Every codex event ever uploaded carries
///       0 tokens: the parser read `payload.input_tokens` but codex nests the
///       counters under `payload.info.last_token_usage`, and an `unwrap_or(0)`
///       turned the miss into a zero. Re-scanning is the ONLY way to recover
///       those numbers — the true counts exist solely in the rollout JSONL on
///       each device, never having reached the server (the log of record faithfully
///       stores the zeros we sent). A re-scan also re-prices every event against
///       the current rate card, which fixes the historical $0.00 costs from the
///       models that had no rate row (core migration 0073).
/// v18 — session metadata on the cloud + SDK paths. Cloud is the DEFAULT mode and
///       its flush branch hardcoded `session_metadata: None`, so the repos / PRs /
///       issues / files a session touched were never sent by anyone — the server's
///       `session_metadata` table held 0 rows, 0 parts, ever. The detection is
///       purely local (event git context, on-disk git, forge refs in the turns), so
///       re-scanning is the ONLY way to recover it for sessions already uploaded:
///       nothing on the server can derive it after the fact. This bump is what
///       makes that automatic — an auto-updated daemon wipes its cursors on first
///       boot and re-processes the world, with no user action.
/// v19 — conversation capture (SPEC 0005). Message excerpts become real bodies:
///       the VERBATIM redacted text of what was said — nothing regexed away, no
///       truncation (the wire cap, raised 320 → 262144, is an extreme
///       malicious-size guard only) — plus `content_bytes`, `turn_index` for
///       claude_code/pi, and Claude Code's own stated `toolUseResult.durationMs`.
///       The server materializes these into the `messages` table and turn-timing
///       columns; everything already uploaded carries only the old 320-char
///       strip-all-code excerpts, so re-scanning the local JSONL corpus is the
///       ONLY way history gains full transcripts. The server dedupes on
///       `(scope, source_event_id)` and ReplacingMergeTree upserts the wider
///       rows over the old ones — the re-scan is pure upgrade.
/// v20 — codex + cursor join conversation capture (SPEC 0005). Codex shipped
///       `content_excerpt: None` on every event, so its sessions produced no
///       `messages` rows and no stance at all; it now carries the typed prompt
///       (`event_msg`/`user_message`) and the assistant's prose (buffered from
///       `event_msg`/`agent_message` onto the usage-bearing `token_count`
///       event, so one event holds both text and tokens, as the other parsers
///       do — codex repeats each message as a `response_item`, which stays
///       text-free so nothing is captured twice). Cursor was worse than empty:
///       it read `ai_code_hashes`, a table current Cursor does not create
///       (absent from every global + workspace DB on a live install), so it
///       could only ever emit nothing; it now reads the real chat store
///       (`cursorDiskKV` bubbles) and emits verbatim user/assistant messages
///       with per-conversation turn ordinals. Both are local-only facts, so a
///       re-scan is the only way sessions already uploaded gain their
///       transcripts.
/// v21 — long turns are actually REDACTED. The on-device NER model carries 512
///       learned positions and errors past them, `classify` mapped that error to
///       "no model", and `pii_redact` reads "no model" as pass-through — so from
///       v19 onward, when turns became verbatim, every turn over ~2,700 chars
///       left the box UNSCRUBBED. (`redactor_active` never caught it: it probes with
///       one short sentinel, which always fits.) Inference is now WINDOWED —
///       every token classified, in overlapping passes the model can take, no
///       text shortened — and the cloud path holds per TURN when the model cannot
///       answer, instead of shipping it. The re-scan is the cleanup: the wider
///       rows upsert over the leaked ones by `(scope, source_event_id)`.
/// v22 — redaction never splices mid-word. A model labels SUBWORDS, and the
///       precise-offset splice took the label at face value, so production shipped
///       `eRPC` as `[REDACTED:ORG]PC`, `Bugbot` as `[REDACTED:ORG]ugbot` and
///       `Compose` as `[REDACTED:ORG]mpose` — 27,130 messages carry an ORG marker,
///       almost all of them technical product names rather than anything private.
///       Reads as corruption of the verbatim text SPEC 0005 exists to keep, and the
///       privacy version is worse: a half-redacted name leaks its other half
///       (`Katherine` → `[REDACTED:PER]erine`). Spans now snap OUTWARD to whole
///       words and fuse when they meet, so a marker never sits against an
///       alphanumeric character. The re-scan is the repair.
/// v23 — the redactor is OpenAI Privacy Filter (ONNX), not a general-purpose NER
///       model. What changes in the DATA: emails, phone numbers, addresses,
///       account numbers and API keys are now caught by the model rather than by
///       the deterministic floor alone; organisations and locations are no longer
///       redacted at all, because an org is not private information and redacting
///       `ClickHouse` cost the prompt analytics for nothing (27,130 of 162,159
///       stored messages carried an ORG marker). Re-ships so history is scrubbed by
///       the model that can actually see secrets, and un-marked where the old one
///       was only ever guessing.
/// The last SINGLE-INTEGER pipeline version (the v1–v23 history above). The
/// integer's flaw was its claim: every bump asserted "all prior output of every
/// parser is stale" even when the change touched one parser (v17: codex token
/// counts) or one aspect (v22: splice only) — and each of the five bumps of
/// early August re-ran the entire corpus on every install. Kept only to migrate
/// stored state; new bumps go in [`ASPECT_VERSIONS`].
pub const LEGACY_WORLD_VERSION: i64 = 23;

/// Per-ASPECT pipeline versions — several exact claims instead of one maximal
/// one. A bump names precisely what today's change invalidated:
///
///   · `capture` / `redaction` — cross-parser aspects; a bump re-reads EVERY
///     file (verbatim capture shape, redaction semantics).
///   · one aspect per parser — a parser-scoped fix (a codex token-counting
///     bug, a cursor schema move) re-reads only that parser's files.
///
/// The interface stays bounded (this fixed key set); the deleted structure is
/// the old implicit claim that any change invalidates the world. All seeded at
/// [`LEGACY_WORLD_VERSION`] so the migration is a no-op for a current install.
///
/// To bump: raise ONE aspect's number and document the why here, exactly as
/// the v1–v23 history did.
///
/// capture v24 — the weakest-hypothesis wave (#108–#112), batched to ONE
///       re-scan on purpose. What history gains by re-reading: unknown record
///       types become visible events (kind verbatim, structural fields only —
///       Desktop's `attachment` rows existed in every transcript and shipped
///       never); codex token-schema drift ships numeric leaves instead of
///       looping a hard-fail forever; pi's absent counters stay absent instead
///       of fabricated zeros, and its providers ship VERBATIM (zhipu was
///       "unknown", which no identity join could ever match); refs carry
///       `ambiguous` instead of two confidence-weighted guesses; path-guessed
///       git slugs stop fabricating `remote_host: "github.com"` and carry
///       `slug_source`; PR outcomes carry the commit + method they were read
///       from; CJK/Cyrillic cognition tags survive; segments carry
///       `local_time`; `mcp.`/`mcp:` tool spellings split correctly (their
///       aggregate keys move). Cross-parser by construction, so the CAPTURE
///       aspect carries the whole wave and the parser aspects stay put — one
///       fleet re-scan, mostly served by the span cache and the cloud
///       classifier.
///
/// claude_code v24 — the durations Claude Code MEASURED, which only the local
///       JSONL ever held. It states its own elapsed time under the name each
///       tool chose, with the unit in that name (`durationMs`,
///       `durationSeconds`, `totalDurationMs` all ship in one release), and the
///       parser read exactly one spelling — so a web search and a sub-agent run,
///       the two longest calls a session makes, reported no duration at all.
///       Also stops dating a turn that states no instant to the epoch: such a
///       line now reports through the skip ledger instead of shipping `ts: ""`,
///       which parses as 1970 and drags every wait derived from it. Re-reading
///       is the only way history gains the numbers; nothing on the server can
///       derive them.
/// codex v24 — the turn ordinal and codex's own turn duration. `turn_index`
///       counted usage-bearing `token_count` lines, i.e. API round trips, so one
///       typed prompt whose reply took three round trips reported three turns
///       and the field meant something different for codex than for every other
///       agent — a cross-agent reading of turn timing cannot survive that. It
///       now advances at the typed prompt, as claude_code, pi and cursor already
///       did. And `task_complete` states `duration_ms`, the only number in a
///       rollout that says how long a turn took; the record has no parser arm,
///       so the number was dropped. A stated duration is structural, like the
///       instant and the ids, so unmodelled records carry it now. Both are
///       local-only facts: a re-scan is the only way uploaded sessions get them.
pub const ASPECT_VERSIONS: &[(&str, i64)] = &[
    ("capture", LEGACY_WORLD_VERSION + 1),
    ("redaction", LEGACY_WORLD_VERSION),
    ("claude_code", LEGACY_WORLD_VERSION + 1),
    ("codex", LEGACY_WORLD_VERSION + 1),
    ("cursor", LEGACY_WORLD_VERSION),
    ("pi", LEGACY_WORLD_VERSION),
];

/// The aspects that invalidate every parser's files when bumped.
const CROSS_PARSER_ASPECTS: [&str; 2] = ["capture", "redaction"];

impl crate::discover_jobs::ParserKind {
    /// The processing aspect this parser's files re-scan under. Exhaustive on
    /// purpose: adding a parser without an [`ASPECT_VERSIONS`] entry fails the
    /// paired test, not a 3 a.m. debugging session.
    pub fn aspect(self) -> &'static str {
        match self {
            crate::discover_jobs::ParserKind::ClaudeCode => "claude_code",
            crate::discover_jobs::ParserKind::Codex => "codex",
            crate::discover_jobs::ParserKind::Pi => "pi",
            crate::discover_jobs::ParserKind::Cursor => "cursor",
        }
    }
}

/// The state a reconcile reads + mutates. Abstracted so the decision is
/// unit-testable without touching `state.json`.
pub trait ProcessingState {
    fn aspect_version(&self, aspect: &str) -> Option<i64>;
    fn set_aspect_version(&mut self, aspect: &str, v: i64);
    /// The pre-aspect single integer, if the state file still carries one.
    fn legacy_processing_version(&self) -> Option<i64>;
    fn clear_legacy_processing_version(&mut self);
    /// Drop every cursor `keep` rejects. `keep(path) == true` retains.
    fn retain_cursors(&mut self, keep: &mut dyn FnMut(&str) -> bool);
}

impl ProcessingState for RuntimeState {
    fn aspect_version(&self, aspect: &str) -> Option<i64> {
        self.processing_aspects.get(aspect).copied()
    }
    fn set_aspect_version(&mut self, aspect: &str, v: i64) {
        self.processing_aspects.insert(aspect.to_string(), v);
    }
    fn legacy_processing_version(&self) -> Option<i64> {
        self.processing_version
    }
    fn clear_legacy_processing_version(&mut self) {
        self.processing_version = None;
    }
    fn retain_cursors(&mut self, keep: &mut dyn FnMut(&str) -> bool) {
        self.cursor.retain(|path, _| keep(path));
    }
}

/// What a reconcile did — surfaced line-by-line in the startup log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VersionReconcile {
    pub changed: bool,
    /// One human line per action taken ("aspect codex v23 → v24: …").
    pub notes: Vec<String>,
}

/// On startup: bring the stored aspect versions up to the compiled ones,
/// wiping exactly the cursors each stale aspect invalidates so the next scan
/// re-reads those files through the current pipeline (a re-scan REPLACES
/// segments/messages by id server-side — no duplicates).
///
/// `parser_of` maps a cursor path to its parser's aspect, from the CURRENT
/// discovery pass — the only honest source of "whose file is this". A path
/// discovery no longer claims wipes CONSERVATIVELY on any parser bump:
/// over-wiping re-reads a file, under-wiping silently skips the repair the
/// bump exists to make.
pub fn reconcile_processing_aspects<S: ProcessingState>(
    state: &mut S,
    parser_of: &dyn Fn(&str) -> Option<&'static str>,
) -> VersionReconcile {
    let mut out = VersionReconcile::default();

    // ── Legacy single-integer migration ──────────────────────────────────
    if let Some(legacy) = state.legacy_processing_version() {
        if legacy < LEGACY_WORLD_VERSION {
            // The old contract for an outdated install: everything re-reads.
            state.retain_cursors(&mut |_| false);
            out.notes.push(format!(
                "legacy pipeline v{legacy} < v{LEGACY_WORLD_VERSION} — wiped every cursor once, \
                 then moved to per-aspect versions"
            ));
        } else {
            out.notes.push(format!(
                "legacy pipeline v{legacy} retired — moved to per-aspect versions, nothing re-read"
            ));
        }
        for (aspect, compiled) in ASPECT_VERSIONS {
            if state.aspect_version(aspect).is_none() {
                state.set_aspect_version(aspect, *compiled);
            }
        }
        state.clear_legacy_processing_version();
        out.changed = true;
    }

    // ── Fresh / hand-edited state: no versions at all ────────────────────
    let any_aspect = ASPECT_VERSIONS
        .iter()
        .any(|(a, _)| state.aspect_version(a).is_some());
    if !any_aspect {
        // No marker anywhere. A fresh install has no cursors (the wipe is
        // free); a state file WITH cursors but no versions is a hand-edit or
        // corruption, and re-reading is the only safe reading of it.
        state.retain_cursors(&mut |_| false);
        for (aspect, compiled) in ASPECT_VERSIONS {
            state.set_aspect_version(aspect, *compiled);
        }
        out.notes
            .push("no pipeline versions stored — seeded all aspects, cursors cleared".into());
        out.changed = true;
        return out;
    }

    // ── Per-aspect bumps ─────────────────────────────────────────────────
    for (aspect, compiled) in ASPECT_VERSIONS {
        let stored = state.aspect_version(aspect).unwrap_or(1);
        if stored >= *compiled {
            continue;
        }
        let mut wiped = 0usize;
        if CROSS_PARSER_ASPECTS.contains(aspect) {
            state.retain_cursors(&mut |_| {
                wiped += 1;
                false
            });
        } else {
            state.retain_cursors(&mut |path| match parser_of(path) {
                Some(a) if a == *aspect => {
                    wiped += 1;
                    false
                }
                // Unclaimed by current discovery: keep only if some OTHER
                // parser claims it; unknown files wipe conservatively.
                Some(_) => true,
                None => {
                    wiped += 1;
                    false
                }
            });
        }
        state.set_aspect_version(aspect, *compiled);
        out.notes.push(format!(
            "aspect {aspect} v{stored} → v{compiled}: {wiped} cursor(s) wiped for re-processing"
        ));
        out.changed = true;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct FakeState {
        legacy: Option<i64>,
        aspects: BTreeMap<String, i64>,
        cursors: Vec<String>,
    }
    impl ProcessingState for FakeState {
        fn aspect_version(&self, aspect: &str) -> Option<i64> {
            self.aspects.get(aspect).copied()
        }
        fn set_aspect_version(&mut self, aspect: &str, v: i64) {
            self.aspects.insert(aspect.into(), v);
        }
        fn legacy_processing_version(&self) -> Option<i64> {
            self.legacy
        }
        fn clear_legacy_processing_version(&mut self) {
            self.legacy = None;
        }
        fn retain_cursors(&mut self, keep: &mut dyn FnMut(&str) -> bool) {
            self.cursors.retain(|p| keep(p));
        }
    }

    /// The compiled version of one aspect. Read rather than written out, so a
    /// bump documents itself in [`ASPECT_VERSIONS`] alone and never has to be
    /// mirrored into an assertion here.
    fn compiled(aspect: &str) -> i64 {
        ASPECT_VERSIONS
            .iter()
            .find(|(a, _)| *a == aspect)
            .map(|(_, v)| *v)
            .expect("aspect exists")
    }

    fn state_with(cursors: &[&str]) -> FakeState {
        FakeState {
            legacy: None,
            aspects: ASPECT_VERSIONS
                .iter()
                .map(|(a, v)| (a.to_string(), *v))
                .collect(),
            cursors: cursors.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Path → aspect for the tests: "/codex/…" is codex's, "/cc/…" is
    /// claude_code's, anything else is unclaimed.
    fn lookup(path: &str) -> Option<&'static str> {
        if path.starts_with("/codex/") {
            Some("codex")
        } else if path.starts_with("/cc/") {
            Some("claude_code")
        } else {
            None
        }
    }

    #[test]
    fn every_parser_has_an_aspect_entry() {
        use crate::discover_jobs::ParserKind::*;
        for kind in [ClaudeCode, Codex, Pi, Cursor] {
            assert!(
                ASPECT_VERSIONS.iter().any(|(a, _)| *a == kind.aspect()),
                "parser {kind:?} has no aspect version — its fixes could never re-scan"
            );
        }
    }

    #[test]
    fn a_current_legacy_install_migrates_without_rereading_anything() {
        // The fleet case on upgrade day: stored v23, aspects absent.
        let mut s = FakeState {
            legacy: Some(LEGACY_WORLD_VERSION),
            aspects: BTreeMap::new(),
            cursors: vec!["/cc/a".into(), "/codex/b".into()],
        };
        let r = reconcile_processing_aspects(&mut s, &lookup);
        assert!(r.changed);
        assert_eq!(
            s.cursors.len(),
            2,
            "a current install must not re-read the world"
        );
        assert_eq!(
            s.legacy, None,
            "the retired integer must not survive a write"
        );
        assert_eq!(s.aspects.len(), ASPECT_VERSIONS.len());
    }

    #[test]
    fn a_stale_legacy_install_rereads_everything_once() {
        let mut s = FakeState {
            legacy: Some(9),
            aspects: BTreeMap::new(),
            cursors: vec!["/cc/a".into()],
        };
        let r = reconcile_processing_aspects(&mut s, &lookup);
        assert!(r.changed);
        assert!(
            s.cursors.is_empty(),
            "the old contract for old installs holds"
        );
        assert_eq!(s.legacy, None);
    }

    #[test]
    fn a_parser_bump_wipes_only_that_parsers_files_and_the_unclaimed() {
        let mut s = state_with(&["/cc/a", "/codex/b", "/mystery/c"]);
        s.aspects.insert("codex".into(), compiled("codex") - 1);
        let r = reconcile_processing_aspects(&mut s, &lookup);
        assert!(r.changed);
        assert_eq!(
            s.cursors,
            vec!["/cc/a".to_string()],
            "codex's file re-reads, the unclaimed file re-reads conservatively, \
             claude_code's file keeps its cursor"
        );
        assert_eq!(s.aspects["codex"], compiled("codex"));
    }

    #[test]
    fn a_cross_parser_bump_rereads_the_world() {
        let mut s = state_with(&["/cc/a", "/codex/b"]);
        s.aspects
            .insert("redaction".into(), compiled("redaction") - 1);
        let r = reconcile_processing_aspects(&mut s, &lookup);
        assert!(r.changed);
        assert!(s.cursors.is_empty());
    }

    #[test]
    fn current_aspects_are_a_noop() {
        let mut s = state_with(&["/cc/a"]);
        let r = reconcile_processing_aspects(&mut s, &lookup);
        assert!(!r.changed, "{:?}", r.notes);
        assert_eq!(s.cursors.len(), 1);
    }

    #[test]
    fn no_versions_at_all_seeds_and_clears() {
        let mut s = FakeState {
            legacy: None,
            aspects: BTreeMap::new(),
            cursors: vec!["/cc/a".into()],
        };
        let r = reconcile_processing_aspects(&mut s, &lookup);
        assert!(r.changed);
        assert!(
            s.cursors.is_empty(),
            "unversioned cursors cannot be trusted"
        );
        assert_eq!(s.aspects.len(), ASPECT_VERSIONS.len());
    }
}
