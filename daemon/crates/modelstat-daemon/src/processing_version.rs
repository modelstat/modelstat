//! Local processing-pipeline version — the MACHINERY.
//!
//! What a stale pipeline declaration does to the file cursors. The declaration
//! itself — every aspect, every generation it has claimed, and the one number
//! the wire states — is [`modelstat_ingest::processing`]; this module only
//! reads it and acts on it.
//!
//! The markers that let a new daemon build force a re-scan of previously
//! uploaded sessions. File cursors track "uploaded up to byte N", so a normal
//! restart only ships new events — but when the pipeline ITSELF changes shape
//! (capture, redaction, a parser's schema handling), the affected output is
//! stale even though the JSONL hasn't moved. On startup the daemon compares
//! the compiled-in PER-ASPECT versions ([`ASPECT_DERIVATIONS`]) to the stored
//! ones; each stale aspect wipes exactly the cursors it invalidates — a
//! parser-scoped fix re-reads one parser's files, a capture/redaction change
//! re-reads the world (a re-scan REPLACES segments/messages by id in place —
//! no duplicates, no orphans).
//!
//! # Only a SEMANTIC bump re-reads anything
//!
//! A stale aspect is not automatically a re-scan. The generations a bump owes
//! each state what they changed, and a span of purely [`Semantics::Mechanical`]
//! ones claims that everything already produced is still correct — so the
//! stored version advances on the spot and not one file is re-read. Wiping
//! there would cost the fleet a full corpus re-read (and an LLM summarise
//! behind each session) to arrive at byte-identical output. Only a
//! [`Semantics::Semantic`] generation in the owed span wipes cursors.
//!
//! # A bump is owed, then honoured — never assumed
//!
//! The stored version does NOT move when the cursors are wiped. It moves in
//! [`settle_processing_rescans`], once a scan has actually re-read every file
//! the bump invalidated; until then the aspect carries a marker in
//! `processingRescans` naming the version it is working toward. So
//! `processingAspects.claude_code == 26` means "every claude_code file has been
//! read by v26 code", not "a v26 binary booted once".
//!
//! That distinction is the whole point. A re-scan of a real corpus spans
//! thousands of files and many sweeps, and the daemon auto-updates, is killed,
//! and is restarted throughout. Stamping the new version at wipe time marks the
//! repair done before a single file has been re-read, so anything that
//! interrupts the pass leaves a device claiming a fix it never applied — and
//! nothing ever revisits it, because the next boot sees stored == compiled and
//! skips. Two states, told apart in writing:
//!
//!   * stored < compiled, no marker  → the bump has not started. Wipe.
//!   * stored < compiled, marker set → it is under way. RESUME; do not wipe
//!     again, or the cursors the interrupted pass earned are thrown away and
//!     the corpus restarts from the top on every boot.
//!
//! Both states are reported: [`rescans_in_progress`] counts what is left and
//! [`rescan_line`] renders it for `modelstat status` and the tray. A skip and
//! a re-scan in progress must never look the same from outside.

use std::collections::BTreeMap;

use modelstat_ingest::processing::{
    aspect_version, replay_owed, Semantics, ASPECT_DERIVATIONS, LEGACY_WORLD_VERSION,
};
use modelstat_ingest::RuntimeState;

/// The aspects that invalidate every parser's files when bumped.
const CROSS_PARSER_ASPECTS: [&str; 2] = ["capture", "redaction"];

/// Does `aspect`'s re-scan claim a file whose parser reports `file_aspect`?
///
/// THE aspect→files mapping, in one place because two callers must agree on it
/// exactly: the cursor wipe that starts a re-scan, and the count that decides
/// the re-scan has finished. If the wipe claimed a file the count did not, the
/// aspect would settle with that file still unread — the silent under-repair
/// the whole mechanism exists to prevent.
fn aspect_owns(aspect: &str, file_aspect: &str) -> bool {
    CROSS_PARSER_ASPECTS.contains(&aspect) || aspect == file_aspect
}

impl crate::discover_jobs::ParserKind {
    /// The processing aspect this parser's files re-scan under. Exhaustive on
    /// purpose: adding a parser without an [`ASPECT_DERIVATIONS`] entry fails
    /// the paired test, not a 3 a.m. debugging session.
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
    /// The version an in-flight re-scan is working toward, if one is.
    fn rescan_target(&self, aspect: &str) -> Option<i64>;
    fn set_rescan_target(&mut self, aspect: &str, v: i64);
    fn clear_rescan_target(&mut self, aspect: &str);
    /// Does this path hold a cursor? A wiped cursor IS the unit of outstanding
    /// re-scan work — the scan re-reads exactly the files that lack one — so
    /// "has a cursor again" is what finishing means, with no second ledger to
    /// drift out of step with the first.
    fn has_cursor(&self, path: &str) -> bool;
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
    fn rescan_target(&self, aspect: &str) -> Option<i64> {
        self.processing_rescans.get(aspect).copied()
    }
    fn set_rescan_target(&mut self, aspect: &str, v: i64) {
        self.processing_rescans.insert(aspect.to_string(), v);
    }
    fn clear_rescan_target(&mut self, aspect: &str) {
        self.processing_rescans.remove(aspect);
    }
    fn has_cursor(&self, path: &str) -> bool {
        self.cursor.contains_key(path)
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
/// Cursors are wiped only where the bump's own declaration says the old output
/// is wrong — see [`reconcile_over`], which this calls with the shipping
/// declaration.
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
    reconcile_over(state, parser_of, ASPECT_DERIVATIONS)
}

/// [`reconcile_processing_aspects`] against an arbitrary declaration.
///
/// The table is a parameter for the same reason [`ProcessingState`] is a trait:
/// the decision has to be provable without the shipping numbers. Every
/// generation this daemon has ever declared is Semantic, so the Mechanical arm
/// — the one that must NOT re-read history — has no live instance to test
/// against, and an arm no test can reach is an arm that quietly stops working.
pub fn reconcile_over<S: ProcessingState>(
    state: &mut S,
    parser_of: &dyn Fn(&str) -> Option<&'static str>,
    aspects: &[(&str, &[Semantics])],
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
        for (aspect, generations) in aspects {
            if state.aspect_version(aspect).is_none() {
                state.set_aspect_version(aspect, aspect_version(generations));
            }
        }
        state.clear_legacy_processing_version();
        out.changed = true;
    }

    // ── Fresh / hand-edited state: no versions at all ────────────────────
    let any_aspect = aspects
        .iter()
        .any(|(a, _)| state.aspect_version(a).is_some());
    if !any_aspect {
        // No marker anywhere. A fresh install has no cursors (the wipe is
        // free); a state file WITH cursors but no versions is a hand-edit or
        // corruption, and re-reading is the only safe reading of it.
        state.retain_cursors(&mut |_| false);
        for (aspect, generations) in aspects {
            state.set_aspect_version(aspect, aspect_version(generations));
        }
        out.notes
            .push("no pipeline versions stored — seeded all aspects, cursors cleared".into());
        out.changed = true;
        return out;
    }

    // ── Per-aspect bumps ─────────────────────────────────────────────────
    for (aspect, generations) in aspects {
        let compiled = aspect_version(generations);
        let stored = state.aspect_version(aspect).unwrap_or(1);
        if stored >= compiled {
            // Nothing owed. Drop a marker a `reset` (or a downgrade) left
            // behind, so no surface advertises a re-scan that cannot happen.
            if state.rescan_target(aspect).is_some_and(|t| t <= stored) {
                state.clear_rescan_target(aspect);
                out.changed = true;
            }
            continue;
        }
        // MECHANICAL span: the bump changed shape, not meaning, so everything
        // already produced is still what this code would produce. Advance the
        // stored version on the spot — there is no work to owe, so no cursor is
        // wiped and no marker is set. Wiping here is what the old bare number
        // did for every bump alike: a fleet-wide re-read of every transcript,
        // and an LLM summarise behind each session, to arrive at output
        // identical to what is already stored.
        if !replay_owed(generations, stored) {
            state.set_aspect_version(aspect, compiled);
            // Cannot happen from a shipped declaration (a marker implies a
            // Semantic generation in the very span just read as Mechanical),
            // but a downgrade or a hand-edited state file can leave one, and a
            // marker outliving its bump advertises a re-scan nothing will ever
            // settle.
            if state.rescan_target(aspect).is_some() {
                state.clear_rescan_target(aspect);
            }
            out.notes.push(format!(
                "aspect {aspect} v{stored} → v{compiled}: mechanical — stored output stands, \
                 nothing re-read"
            ));
            out.changed = true;
            continue;
        }
        // Already re-scanning toward exactly this version. Wiping again would
        // throw away the cursors the interrupted pass EARNED and restart the
        // corpus from the top on every boot — a re-scan that never converges
        // looks exactly like a daemon stuck in a loop. Resume instead: the
        // files still missing a cursor are precisely the ones still owed.
        if state.rescan_target(aspect) == Some(compiled) {
            out.notes.push(format!(
                "aspect {aspect} v{stored} → v{compiled}: re-scan already under way — resuming"
            ));
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
        // The stored version deliberately does NOT move here. It advances in
        // [`settle_processing_rescans`], once a scan has actually re-read every
        // file this bump invalidated. Stamping it now would mark the repair done
        // before a single file had been re-read, and a daemon killed mid-pass —
        // or simply auto-updated again, which is how this code path is usually
        // reached — would never revisit the remainder.
        state.set_rescan_target(aspect, compiled);
        out.notes.push(format!(
            "aspect {aspect} v{stored} → v{compiled}: {wiped} cursor(s) wiped — re-scan started"
        ));
        out.changed = true;
    }
    out
}

/// A re-scan a version bump mandated and the scan has not finished yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RescanProgress {
    pub aspect: &'static str,
    /// The STORED version — still the old one until the re-scan drains.
    pub from: i64,
    /// The compiled version the re-scan is working toward.
    pub to: i64,
    /// Discovered files this aspect owns that are still missing a cursor.
    pub files_left: usize,
}

impl std::fmt::Display for RescanProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "re-scanning for {} v{}→v{}, ",
            self.aspect, self.from, self.to
        )?;
        match self.files_left {
            1 => f.write_str("last file"),
            n => write!(f, "{} files left", crate::runtime::thousands(n as u64)),
        }
    }
}

/// Every re-scan still owed work, with how much of it is left.
///
/// `discovered` is path → aspect from the CURRENT discovery pass. A cursor path
/// discovery no longer claims is absent from it ON PURPOSE: a transcript deleted
/// since the wipe can never be re-read, so counting it would pin the re-scan
/// open forever and re-wipe the corpus on every boot. The local file is the
/// source and may vanish; nothing here treats its absence as anything but "no
/// work to do" — the retention invariant is stated in full at the GC seam in
/// [`crate::reconcile`].
pub fn rescans_in_progress<S: ProcessingState>(
    state: &S,
    discovered: &BTreeMap<String, &'static str>,
) -> Vec<RescanProgress> {
    ASPECT_DERIVATIONS
        .iter()
        .filter_map(|(aspect, _)| {
            let to = state.rescan_target(aspect)?;
            Some(RescanProgress {
                aspect,
                from: state.aspect_version(aspect).unwrap_or(1),
                to,
                files_left: discovered
                    .iter()
                    .filter(|(path, fa)| aspect_owns(aspect, fa) && !state.has_cursor(path))
                    .count(),
            })
        })
        .collect()
}

/// Advance every aspect whose re-scan has ACTUALLY finished. The stored version
/// moves here and nowhere else, so "the state file says v26" means "every file
/// v26 invalidated has been read by v26 code" rather than "a v26 binary booted
/// once". Call it when a scan sweep has drained — that is the only moment the
/// daemon knows nothing is still queued.
pub fn settle_processing_rescans<S: ProcessingState>(
    state: &mut S,
    discovered: &BTreeMap<String, &'static str>,
) -> VersionReconcile {
    let mut out = VersionReconcile::default();
    for p in rescans_in_progress(state, discovered) {
        if p.files_left > 0 {
            continue;
        }
        state.set_aspect_version(p.aspect, p.to);
        state.clear_rescan_target(p.aspect);
        out.notes.push(format!(
            "aspect {} v{} → v{}: re-scan complete",
            p.aspect, p.from, p.to
        ));
        out.changed = true;
    }
    out
}

/// The one line the status surfaces give a re-scan, or `None` when none is owed.
///
/// `None` rather than a "0 files left" row: a surface that keeps rendering a
/// finished re-scan is the same failure as a daemon that logs "nothing to do"
/// while three thousand files wait — a reader cannot tell a no-op from work.
pub fn rescan_line(pending: &[RescanProgress]) -> Option<String> {
    if pending.is_empty() {
        return None;
    }
    Some(
        pending
            .iter()
            .map(RescanProgress::to_string)
            .collect::<Vec<_>>()
            .join("; "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct FakeState {
        legacy: Option<i64>,
        aspects: BTreeMap<String, i64>,
        rescans: BTreeMap<String, i64>,
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
        fn rescan_target(&self, aspect: &str) -> Option<i64> {
            self.rescans.get(aspect).copied()
        }
        fn set_rescan_target(&mut self, aspect: &str, v: i64) {
            self.rescans.insert(aspect.into(), v);
        }
        fn clear_rescan_target(&mut self, aspect: &str) {
            self.rescans.remove(aspect);
        }
        fn has_cursor(&self, path: &str) -> bool {
            self.cursors.iter().any(|p| p == path)
        }
    }

    /// The compiled version of one aspect. Read rather than written out, so a
    /// bump documents itself in [`ASPECT_DERIVATIONS`] alone and never has to
    /// be mirrored into an assertion here.
    fn compiled(aspect: &str) -> i64 {
        ASPECT_DERIVATIONS
            .iter()
            .find(|(a, _)| *a == aspect)
            .map(|(_, generations)| aspect_version(generations))
            .expect("aspect exists")
    }

    fn state_with(cursors: &[&str]) -> FakeState {
        FakeState {
            legacy: None,
            aspects: ASPECT_DERIVATIONS
                .iter()
                .map(|(a, generations)| (a.to_string(), aspect_version(generations)))
                .collect(),
            rescans: BTreeMap::new(),
            cursors: cursors.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A synthetic declaration: one parser-scoped aspect whose newest
    /// generation states `kind`, everything else current. Lets the two arms be
    /// exercised against the real machinery while every SHIPPING generation is
    /// still Semantic.
    fn declaring(
        aspect: &'static str,
        kind: Semantics,
    ) -> Vec<(&'static str, &'static [Semantics])> {
        let bumped: &'static [Semantics] = match kind {
            Semantics::Semantic => &[Semantics::Semantic, Semantics::Semantic],
            Semantics::Mechanical => &[Semantics::Semantic, Semantics::Mechanical],
        };
        ASPECT_DERIVATIONS
            .iter()
            .map(|(a, generations)| {
                if *a == aspect {
                    (*a, bumped)
                } else {
                    (*a, *generations)
                }
            })
            .collect()
    }

    /// The state a device is in one release BEFORE `table` — every aspect at
    /// its declared version except the one that just bumped.
    fn state_one_behind(
        table: &[(&str, &[Semantics])],
        aspect: &str,
        cursors: &[&str],
    ) -> FakeState {
        FakeState {
            legacy: None,
            aspects: table
                .iter()
                .map(|(a, generations)| {
                    let v = aspect_version(generations);
                    (a.to_string(), if *a == aspect { v - 1 } else { v })
                })
                .collect(),
            rescans: BTreeMap::new(),
            cursors: cursors.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// What discovery would report for these paths, by the same rule
    /// [`lookup`] uses. Unclaimed paths are simply absent — discovery never
    /// reports a file it cannot see.
    fn discovered(paths: &[&str]) -> BTreeMap<String, &'static str> {
        paths
            .iter()
            .filter_map(|p| lookup(p).map(|a| ((*p).to_string(), a)))
            .collect()
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
                ASPECT_DERIVATIONS.iter().any(|(a, _)| *a == kind.aspect()),
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
            rescans: BTreeMap::new(),
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
        assert_eq!(s.aspects.len(), ASPECT_DERIVATIONS.len());
    }

    #[test]
    fn a_stale_legacy_install_rereads_everything_once() {
        let mut s = FakeState {
            legacy: Some(9),
            aspects: BTreeMap::new(),
            rescans: BTreeMap::new(),
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
        assert_eq!(
            s.aspects["codex"],
            compiled("codex") - 1,
            "the stored version stays PUT until the re-scan actually runs"
        );
        assert_eq!(
            s.rescans["codex"],
            compiled("codex"),
            "…and is owed, in writing"
        );
    }

    #[test]
    fn a_bump_is_not_marked_done_until_the_rescan_finishes() {
        let mut s = state_with(&["/codex/b"]);
        s.aspects.insert("codex".into(), compiled("codex") - 1);
        reconcile_processing_aspects(&mut s, &lookup);

        // The wipe happened; the file is owed a read and the surfaces say so.
        let disc = discovered(&["/codex/b"]);
        let pending = rescans_in_progress(&s, &disc);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].aspect, "codex");
        assert_eq!(pending[0].files_left, 1);
        assert_eq!(
            rescan_line(&pending).as_deref(),
            Some(&*format!(
                "re-scanning for codex v{}→v{}, last file",
                compiled("codex") - 1,
                compiled("codex")
            )),
        );

        // Settling now would be a lie — nothing has been re-read.
        let r = settle_processing_rescans(&mut s, &disc);
        assert!(!r.changed);
        assert_eq!(s.aspects["codex"], compiled("codex") - 1);

        // The scan re-reads it, which is exactly "the cursor came back".
        s.cursors.push("/codex/b".into());
        let r = settle_processing_rescans(&mut s, &disc);
        assert!(r.changed);
        assert_eq!(s.aspects["codex"], compiled("codex"));
        assert!(s.rescans.is_empty());
        assert_eq!(rescan_line(&rescans_in_progress(&s, &disc)), None);
    }

    #[test]
    fn an_interrupted_rescan_resumes_rather_than_restarting() {
        let mut s = state_with(&["/codex/a", "/codex/b"]);
        s.aspects.insert("codex".into(), compiled("codex") - 1);
        reconcile_processing_aspects(&mut s, &lookup);
        assert!(s.cursors.is_empty(), "both codex files were wiped");

        // The daemon re-read one file, then died (or auto-updated again).
        s.cursors.push("/codex/a".into());

        // Next boot: the stored version is still behind, so the reconcile runs
        // again — and must NOT wipe the work the interrupted pass earned.
        let r = reconcile_processing_aspects(&mut s, &lookup);
        assert_eq!(
            s.cursors,
            vec!["/codex/a".to_string()],
            "a resumed re-scan keeps the cursors it already earned"
        );
        assert!(
            r.notes.iter().any(|n| n.contains("resuming")),
            "{:?}",
            r.notes
        );
        assert_eq!(
            rescans_in_progress(&s, &discovered(&["/codex/a", "/codex/b"]))[0].files_left,
            1,
            "one file still owed, not two"
        );
    }

    #[test]
    fn a_parser_bump_leaves_the_other_parsers_alone() {
        let mut s = state_with(&["/cc/a", "/codex/b"]);
        s.aspects.insert("codex".into(), compiled("codex") - 1);
        reconcile_processing_aspects(&mut s, &lookup);
        let disc = discovered(&["/cc/a", "/codex/b"]);
        let pending = rescans_in_progress(&s, &disc);
        assert_eq!(
            pending.len(),
            1,
            "a codex bump owes nothing for claude_code"
        );
        assert_eq!(pending[0].aspect, "codex");
        assert_eq!(
            pending[0].files_left, 1,
            "and counts only codex's file, though claude_code's was never wiped"
        );
    }

    #[test]
    fn a_cross_parser_rescan_counts_every_parsers_files() {
        let mut s = state_with(&["/cc/a", "/codex/b"]);
        s.aspects.insert("capture".into(), compiled("capture") - 1);
        reconcile_processing_aspects(&mut s, &lookup);
        let disc = discovered(&["/cc/a", "/codex/b"]);
        let pending = rescans_in_progress(&s, &disc);
        assert_eq!(pending[0].files_left, 2, "capture owns the world");
        assert!(rescan_line(&pending).unwrap().contains("2 files left"));
    }

    /// The retention invariant, at the seam that would be tempted to break it:
    /// a transcript deleted between scans is simply not discovered. It must not
    /// hold its aspect's re-scan open — which would re-wipe and re-read the
    /// whole corpus on every boot, forever — and it must not be mistaken for
    /// outstanding work. Absence means "nothing to read", never "something to
    /// undo": the server keeps that session either way.
    #[test]
    fn a_transcript_deleted_between_scans_never_pins_a_rescan_open() {
        let mut s = state_with(&["/codex/kept", "/codex/deleted"]);
        s.aspects.insert("codex".into(), compiled("codex") - 1);
        reconcile_processing_aspects(&mut s, &lookup);

        // The user's tool pruned `/codex/deleted`; discovery no longer sees it.
        let disc = discovered(&["/codex/kept"]);
        s.cursors.push("/codex/kept".into());

        let r = settle_processing_rescans(&mut s, &disc);
        assert!(
            r.changed,
            "the re-scan is complete — a file that no longer exists cannot be re-read"
        );
        assert_eq!(s.aspects["codex"], compiled("codex"));
        assert!(rescans_in_progress(&s, &disc).is_empty());
    }

    #[test]
    fn a_stale_rescan_marker_is_dropped_rather_than_advertised() {
        // What `modelstat reset` (or a downgrade) leaves behind: the stored
        // version is already current, so the marker names work nobody owes.
        let mut s = state_with(&["/codex/b"]);
        s.rescans.insert("codex".into(), compiled("codex"));
        let r = reconcile_processing_aspects(&mut s, &lookup);
        assert!(r.changed);
        assert!(s.rescans.is_empty());
        assert_eq!(
            rescan_line(&rescans_in_progress(&s, &discovered(&["/codex/b"]))),
            None
        );
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
            rescans: BTreeMap::new(),
            cursors: vec!["/cc/a".into()],
        };
        let r = reconcile_processing_aspects(&mut s, &lookup);
        assert!(r.changed);
        assert!(
            s.cursors.is_empty(),
            "unversioned cursors cannot be trusted"
        );
        assert_eq!(s.aspects.len(), ASPECT_DERIVATIONS.len());
    }

    /// Direction one: a bump whose newest generation states `Semantic` claims
    /// the old output is wrong, so the cursors it owns go and the version waits
    /// on the re-read. This is what every generation shipped so far does.
    #[test]
    fn a_semantic_bump_replays_history() {
        let table = declaring("codex", Semantics::Semantic);
        let mut s = state_one_behind(&table, "codex", &["/cc/a", "/codex/b"]);
        let before = compiled_in(&table, "codex") - 1;

        let r = reconcile_over(&mut s, &lookup, &table);

        assert!(r.changed);
        assert_eq!(
            s.cursors,
            vec!["/cc/a".to_string()],
            "codex's file must be re-read; claude_code's must not"
        );
        assert_eq!(
            s.aspects["codex"], before,
            "the version waits on the re-read that was just ordered"
        );
        assert_eq!(
            s.rescans["codex"],
            before + 1,
            "…and the work is owed, in writing"
        );
        assert!(
            r.notes.iter().any(|n| n.contains("re-scan started")),
            "{:?}",
            r.notes
        );
    }

    /// Direction two: the same bump, declared `Mechanical`. The shape moved and
    /// the meaning did not, so every stored output is still correct — not one
    /// cursor may be dropped, and the version advances immediately because
    /// there is nothing to wait for. Under the old bare number this case was
    /// indistinguishable from the one above and cost the whole fleet a corpus
    /// re-read (plus an LLM summarise per session) for byte-identical output.
    #[test]
    fn a_mechanical_bump_does_not_replay_history() {
        let table = declaring("codex", Semantics::Mechanical);
        let mut s = state_one_behind(&table, "codex", &["/cc/a", "/codex/b"]);
        let target = compiled_in(&table, "codex");

        let r = reconcile_over(&mut s, &lookup, &table);

        assert!(r.changed, "the version still moves");
        assert_eq!(
            s.cursors,
            vec!["/cc/a".to_string(), "/codex/b".to_string()],
            "a Mechanical bump must re-read NOTHING — every cursor stands"
        );
        assert_eq!(
            s.aspects["codex"], target,
            "and settles at once: no re-read to wait for"
        );
        assert!(
            s.rescans.is_empty(),
            "no re-scan is owed, so no surface may advertise one"
        );
        assert!(
            r.notes.iter().any(|n| n.contains("mechanical")),
            "the reason is stated in the startup log: {:?}",
            r.notes
        );
        assert_eq!(
            rescan_line(&rescans_in_progress(
                &s,
                &discovered(&["/cc/a", "/codex/b"])
            )),
            None
        );
    }

    /// A device that skipped a release arrives owing several generations at
    /// once. The Semantic one among them is honoured even when the NEWEST is
    /// Mechanical — reading only the latest would silently drop the repair.
    #[test]
    fn a_mechanical_bump_still_replays_a_semantic_one_it_skipped() {
        let skipped: &'static [Semantics] = &[
            Semantics::Semantic,
            Semantics::Semantic,
            Semantics::Mechanical,
        ];
        let table: Vec<(&str, &[Semantics])> = vec![("codex", skipped)];
        let mut s = FakeState {
            legacy: None,
            // Two generations behind: one Semantic, then one Mechanical.
            aspects: BTreeMap::from([("codex".to_string(), LEGACY_WORLD_VERSION + 1)]),
            rescans: BTreeMap::new(),
            cursors: vec!["/codex/b".into()],
        };

        reconcile_over(&mut s, &lookup, &table);

        assert!(
            s.cursors.is_empty(),
            "the skipped Semantic generation still owes a re-read"
        );
        assert_eq!(s.rescans["codex"], LEGACY_WORLD_VERSION + 3);
    }

    /// The compiled version of one aspect within a given declaration.
    fn compiled_in(table: &[(&str, &[Semantics])], aspect: &str) -> i64 {
        table
            .iter()
            .find(|(a, _)| *a == aspect)
            .map(|(_, generations)| aspect_version(generations))
            .expect("aspect exists")
    }
}
