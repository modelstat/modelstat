//! Local processing-pipeline version.
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
/// codex v25 — which files the work actually touched. `files_touched` had no
///       producer in ANY parser — every construction site was `Vec::new()`, and
///       the only non-empty occurrences in the tree were test fixtures — so the
///       taxonomy `components` dimension the server derives from it
///       (`components_from_slice`, one `hint("components", …, 0.6)` per value)
///       has been computed over an empty list on every session, from every
///       agent, for as long as the field has existed. Codex is the agent that
///       STATES the answer: `event_msg`/`patch_apply_end` keys `payload.changes`
///       by the path of each file it just edited (1,019 such records in one real
///       session), and the parser dropped the whole record as unmodelled. It now
///       ships as an event carrying those paths, made safe where they are read:
///       relative to the session's `cwd` when they sit under it, the file's name
///       alone when they do not, so no home directory ever leaves the machine.
///       The unified diffs in that payload stay on disk. Only the codex aspect
///       moves — ~65 codex sessions re-read, not the corpus.
/// codex v26 — a round trip's identity. A `codex resume`/subagent-spawn rollout
///       opens with its own `session_meta`, then REPLAYS the ancestor's whole
///       history — and codex rewrites every copied timestamp to the fork moment,
///       so a copy shares no position and no instant with its original. Keyed on
///       `(file, byte offset)`, each replay minted a brand-new event under the
///       ancestor's session id, and one 14-day conversation forked 448 times billed
///       itself 51x (466.9B of a 489.7B account). Claude Code and cursor were never
///       exposed: their copies keep the line uuid, so the store collapses them.
///       Codex gives a line no uuid, but `total_token_usage` — the conversation's
///       running total — survives a copy byte for byte, so it is the key now. A
///       re-scan is what re-keys the history; the server tombstones the events the
///       old key already landed. Rides the SAME codex bump wave as v25 where it
///       can — but v25 landed first, so this is its own re-read.
/// claude_code v25 — the model's own THINKING, on transcripts already read.
///       Claude Code writes reasoning as `thinking` content blocks beside the
///       `text` ones, and the excerpt pass filtered to `text` — so every session
///       that reasoned shipped no trace of having reasoned, and the block's
///       opaque `signature` is the only part of it nothing wants. Turns now
///       carry `reasoning_excerpt`/`reasoning_bytes` (redacted on the same
///       fail-closed path as the prose; the signature stays behind). Re-reading
///       is the only way history gains it: the blocks exist solely in the local
///       JSONL. The 1,694 sub-agent transcripts this release starts walking need
///       no bump — they have no cursors to wipe — but the SAME re-read is what
///       makes existing main transcripts give up their thinking.
/// codex v27 — the multi-agent run, which was three unmodelled record types.
///       `event_msg`/`sub_agent_activity` is the sub-agent LIFECYCLE codex
///       states about itself (2,132 records in one real rollout: 446 starts,
///       1,649 interactions, 37 interruptions across 440 agents) and it dropped
///       whole. `response_item`/`agent_message` is the traffic BETWEEN those
///       agents — author, recipient, and what was said — and 3,109 of them
///       shipped as content-free unknown records; it has no `event_msg` twin, so
///       capturing it doubles nothing. `event_msg`/`agent_reasoning` is the
///       model's thinking, 14,802 records buffered onto the round trip's own
///       usage-bearing event exactly as its prose already was. All three are
///       local-only facts, so a re-scan is the only way uploaded sessions get
///       them; only codex's own files re-read.
/// capture v25 — segments state which SCAN produced them (core#701). The scan
///       flushes every `BATCH_MAX_EVENTS`, so a session's segmentation leaves in
///       several batches, and the server inferred what a batch superseded from
///       TIME OVERLAP. A cursor-resumed scan overlaps older segments without
///       re-stating them, so the server retired spans no batch ever restated:
///       116 sessions and 50,651,192,068 tokens — 29.5% of all measured work —
///       ended up with no live segment at all, invisible to every taxonomy,
///       insight and node-spend read while the rollups kept counting the events.
///       Batches now carry `segment_generations`, and this bump is the repair:
///       only a re-read regenerates the segments that were retired without a
///       replacement. Cross-parser, because the loss is in supersession rather
///       than in any one parser — claude_code and pi were worst hit.
/// capture v26 — tool calls carry their end instants, paired from what the
///       logs state. A `ToolCallWire`'s `ended_at` is how the server splits a
///       turn into model thinking vs waiting on tools, and history uploaded by
///       earlier builds carries it almost nowhere — so tool wait read as ~0
///       everywhere. Each source states the end in its own place and the
///       parsers now read all of them: Claude Code's `tool_result` line dates
///       the call it answers (and an UNDATED result line no longer stamps
///       `ended_at: ""`, which parses as the epoch); codex's
///       `*_tool_call_output` records already dated their calls, and its
///       `event_msg`/`mcp_tool_call_end` — until now an unmodelled kind that
///       only warned — states a whole MCP call by itself (invocation, result,
///       end instant, measured duration), so those calls exist at all now;
///       pi's `toolResult` line already dated its call. Cursor's bubble store
///       states tool status but no end instant, so cursor honestly emits none
///       (missing means unknown; a fabricated end poisons the decomposition).
///       Rides with it: codex's `thread_goal_updated` and `turn_aborted`
///       become modelled events instead of warnings, and
///       `inter_agent_communication_metadata` (one undocumented boolean) is
///       consumed as a decision instead of ledgered noise. All of it is
///       local-only fact: only a re-read fills the historical spans, and the
///       deterministic `tc_` ids mean the server upserts the ends onto the
///       calls it already has. Cross-parser, so the CAPTURE aspect carries it.
/// capture v27 — provenance tiering for repo slugs. Segments derive their
///       `projects` hint from stored events at scan time, and that derivation
///       changed shape: the hint now reads the first VERIFIED slug in the
///       slice (not blindly the first event), tiers its confidence by
///       `slug_source` — with a real `remote_url` accepted as evidence on
///       pre-marker events, since no guess path ever wrote one — and ships the
///       marker verbatim as the hint's `reason`; session-metadata repo refs
///       ship `git` vs `git_guess` from the same predicate. All of it is
///       re-derived from the local logs, so a re-read is the only way
///       historical segments gain the tiering. Cross-parser (every parser's
///       events carry git context), so the CAPTURE aspect carries it.
/// codex v28 — a rollout file is its OWN session. A fork (`codex resume`, a
///       subagent spawn) opens with its own `session_meta` naming its own uuid,
///       then REPLAYS the ancestor's history — the ancestor's `session_meta`
///       included — and the parser took every declared id as identity. So each
///       fork rebound to its ancestor the moment the replay began, and its own
///       new work landed there too: on one real machine 447 of 485 rollout files
///       collapsed into ONE session holding 7,339,890 event rows spanning two
///       weeks — 64% of the entire events table. That session exceeds every
///       processing ceiling, so it yielded no tasks, no taxonomy and no
///       attribution, while its 9,138 uploads starved the summarise queue. The
///       filename's uuid is the identity now; a declared id that disagrees is an
///       ancestor pointer, and the payload wins only when the path names nobody.
///       Round-trip KEYS are untouched — they still hash the conversation the
///       region declares, which is what makes a replayed turn collapse onto the
///       original (v26) rather than bill twice. A re-scan is what re-keys the
///       history to the sessions that actually did the work: the events the old
///       binding landed are the server's to tombstone. Only codex's files
///       re-read.
/// claude_code v26 — a transcript record is identified by its own `uuid`.
///       `--resume`/`--continue` writes a new transcript that opens with copies
///       of the ancestor's records, and the parser recognised a copy by the one
///       thing it used to state: a `sessionId` that disagreed with the filename.
///       Current Claude Code REWRITES that field to the new file's own uuid, so
///       a copy is byte-identical to its original except for the single field
///       the rule read — and the rule stopped firing. Keyed on
///       `(file, byte offset)`, every replayed record then minted a fresh id:
///       on one real machine 14,865 of 413,066 emitted records were the same
///       line read out of two transcripts, across 32 files, each of them
///       counted twice — 3.6% of the corpus, and concentrated in whichever
///       sessions the user resumed most, which are the long ones. The `uuid`
///       survives every copy shape byte for byte (verified: the full record
///       diff between a copy and its original is that one field), so it is the
///       key now, and a record that states no uuid still falls back to its
///       position. It also collects a second shape the positional key could
///       never see: 15 transcripts on that machine re-append a record they
///       already hold — same uuid, same `requestId`, same `message.id`, same
///       counters — 9,553 lines whose tokens were billed twice. The ancestor probe stays for the OLD shape alone, deciding
///       only whether a copy that still declares its ancestor is worth
///       emitting. Session identity is untouched — a transcript's records
///       belong to the session its filename names, as codex v28 established. A
///       re-scan is what re-keys the history; the events the old key landed are
///       the server's to tombstone. Only claude_code's files re-read.
/// codex v29 — the records a fork replays, keyed on what codex CALLS them.
///       v26 keyed the token-bearing round trips and v25's `patch_apply_end`
///       keyed on `call_id`, but every other codex record still hashed
///       `(file, byte offset)` — and a fork replays the ancestor's whole
///       history at a fresh offset with a rewritten timestamp, so each copy
///       minted a brand-new event. On one real machine of 485 rollout files
///       that is 454,155 of the 467,750 records now covered: the same work
///       counted up to 348 times, worst in the multi-agent runs, where 437,195
///       `sub_agent_activity` lines are 5,498 real lifecycle events and 431,697
///       replays of them. Also 3,109 duplicate inter-agent `agent_message`
///       records, 6,350 duplicate typed prompts (6,491 records are 136 prompts,
///       because every fork replays the root prompt), and 13,000 duplicate
///       `web_search_end` / `task_started` / `task_complete` records.
///       Codex states an id on these records and it survives a copy byte for
///       byte — the FIELD differs by record type (`call_id`, `client_id`,
///       `event_id`, `turn_id`, `id`, or `turn_id` nested in the passthrough
///       envelope), so one narrowest-first vocabulary is tried against every
///       record instead of a table of type→field. An id alone is not a record:
///       it names a CONTAINER, so the key is `(stated id, record type,
///       ordinal under that id in this file)` — the type because a turn's
///       `task_started` and `task_complete` state the same uuid, the ordinal
///       because one turn carries up to 157 inter-agent messages and a replay
///       copies them in order. Verified against the whole corpus: no key value
///       covers two records that differ in any field.
///       NOT content-keyed, though a content hash was the obvious candidate:
///       codex rewrites a record between copies — an observed `turn_aborted`
///       ships once with `completed_at`/`duration_ms` and once without — so
///       hashing content splits one event in two, and it would also fuse the
///       same prompt genuinely typed twice. Records that state no id keep their
///       position, which is the honest answer rather than a lesser one:
///       `context_compacted` is the literal payload `{"type":"..."}` (40,101 of
///       them), and `thread_settings_applied` and `thread_goal_updated` name
///       nothing either. `turn_aborted` also stays positional — it is the one
///       record type whose content is known to vary between copies, so its
///       key needs evidence this corpus does not give.
///       A re-scan is what re-keys the history; the events the old key already
///       landed are the server's to tombstone. Only codex's files re-read.
/// capture v28 — a session's repository is a fact about the FILES it touched,
///       not about where the agent happened to be started. Placement routes on
///       the segment `projects` hint, that hint takes only a VERIFIED slug
///       (`git_remote` or `repo_root_dir`), and the daemon derived the slug by
///       resolving `cwd` and nothing else — so an agent launched from a
///       directory that HOLDS checkouts rather than being one stated no repo at
///       all, on every session, forever. Measured over all time: `pi` stated a
///       repo on 0 of 327 sessions, `cursor` on 0 of 202, Claude Desktop on 0
///       of 51, against `claude_code`'s 1,131 of 1,147 — the one agent normally
///       launched from inside the checkout. Downstream that is not a missing
///       label but a wrong home: 306 of one user's 317 sessions fell through to
///       their router's personal workspace instead of the org that owns the
///       code they were editing. Events now carry the paths their tool calls
///       named — local-only, shed at the wire door beside `cwd`, because an
///       absolute path is a home directory — and resolution tries the parent
///       directory of each of them, most specific first, before falling back to
///       `cwd`. CROSS-PARSER: claude_code, codex and pi all produce the paths,
///       and every parser's events are re-filed by the corrected identity, so
///       the CAPTURE aspect carries it rather than any parser aspect. A FLEET
///       RE-SCAN is required and is the whole point — `git.slug_source` is the
///       documented re-ship trigger (`consumer/src/backfill_segment_hints.rs`
///       re-ships a session when it changes), and re-reading the local logs is
///       the only thing that can re-file a session already uploaded: the server
///       cannot derive a repo from data that never named one. Cursor is not a
///       producer here — it parses no tool calls, so it has no paths to offer
///       and its sessions still wait on a source of their own.
pub const ASPECT_VERSIONS: &[(&str, i64)] = &[
    ("capture", LEGACY_WORLD_VERSION + 5),
    ("redaction", LEGACY_WORLD_VERSION),
    ("claude_code", LEGACY_WORLD_VERSION + 3),
    ("codex", LEGACY_WORLD_VERSION + 6),
    ("cursor", LEGACY_WORLD_VERSION),
    ("pi", LEGACY_WORLD_VERSION),
];

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
            // Nothing owed. Drop a marker a `reset` (or a downgrade) left
            // behind, so no surface advertises a re-scan that cannot happen.
            if state.rescan_target(aspect).is_some_and(|t| t <= stored) {
                state.clear_rescan_target(aspect);
                out.changed = true;
            }
            continue;
        }
        // Already re-scanning toward exactly this version. Wiping again would
        // throw away the cursors the interrupted pass EARNED and restart the
        // corpus from the top on every boot — a re-scan that never converges
        // looks exactly like a daemon stuck in a loop. Resume instead: the
        // files still missing a cursor are precisely the ones still owed.
        if state.rescan_target(aspect) == Some(*compiled) {
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
        state.set_rescan_target(aspect, *compiled);
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
    ASPECT_VERSIONS
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
        assert_eq!(s.aspects.len(), ASPECT_VERSIONS.len());
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
        assert_eq!(s.aspects.len(), ASPECT_VERSIONS.len());
    }
}
