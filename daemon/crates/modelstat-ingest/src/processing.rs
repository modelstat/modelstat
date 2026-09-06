//! The local processing pipeline, DECLARED — what each generation of it
//! claimed, and the one number the wire states for it.
//!
//! This is the declaration half of the pipeline-version story. The other half —
//! what a stale declaration does to the file cursors — is the daemon's
//! `processing_version` module, which reads this table and acts on it. They are
//! split because two crates need to READ the declaration (both batch builders,
//! to state it on the wire) and only one needs to act on it.
//!
//! # A bump states what it CHANGED, not just that it changed
//!
//! A version bump used to be a bare number, and a bare number can only make the
//! maximal claim: everything produced by older code is stale, re-read the
//! corpus. That is right for a redaction rule that now scrubs differently and
//! wrong for a serialization fix, and the number cannot tell the two apart — so
//! every bump paid the full price of the most expensive kind of bump.
//!
//! So a generation is not a number here. It is a [`Semantics`], and the version
//! is how many of them an aspect has declared ([`aspect_version`]). There is no
//! number to raise on its own: a bump is an appended generation, and a
//! generation states its kind or it does not compile.

/// What a bump CLAIMS about the local output already produced — stated at the
/// bump site, never inferred.
///
/// The same distinction the server draws for its own derivations, on the
/// daemon's own axis: the two sides run different code over different inputs,
/// but "did the JUDGEMENT change, or only the SHAPE?" is one question and it
/// deserves one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Semantics {
    /// The OUTPUT MEANING changed — a parser now reads a field it dropped, a
    /// redaction rule scrubs differently, a segment carries a value derived
    /// another way. What the old code produced is not what this code would
    /// produce from the same bytes, so history is stale and only a re-read
    /// repairs it: the local transcript is the sole place the truth exists.
    Semantic,
    /// The SHAPE changed — a field moved on the wire, a plumbing fix, a rename.
    /// Everything already produced is still exactly what this code would
    /// produce, so re-reading the corpus would buy nothing and cost a full
    /// fleet-wide re-scan (thousands of files per device, an LLM summarise
    /// behind each one). The stored outputs stand; only the number moves.
    Mechanical,
}

/// The last SINGLE-INTEGER pipeline version (the v1–v23 history the daemon's
/// `processing_version` module records). The integer's flaw was its claim:
/// every bump asserted "all prior output of every parser is stale" even when
/// the change touched one parser (v17: codex token counts) or one aspect (v22:
/// splice only) — and each of the five bumps of early August re-ran the entire
/// corpus on every install. Kept as the base every aspect counts from, so a
/// generation number here reads on the same axis the old integer used.
///
/// The history it carried, kept because these generations really happened and
/// a device can still arrive stored at one of them:
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
pub const LEGACY_WORLD_VERSION: i64 = 23;

/// `capture` — a cross-parser aspect: a bump re-reads EVERY parser's files.
const CAPTURE: &[Semantics] = &[
    // v24 — the weakest-hypothesis wave (#108–#112), batched to ONE re-scan on
    // purpose. What history gains by re-reading: unknown record types become
    // visible events (kind verbatim, structural fields only — Desktop's
    // `attachment` rows existed in every transcript and shipped never); codex
    // token-schema drift ships numeric leaves instead of looping a hard-fail
    // forever; pi's absent counters stay absent instead of fabricated zeros, and
    // its providers ship VERBATIM (zhipu was "unknown", which no identity join
    // could ever match); refs carry `ambiguous` instead of two
    // confidence-weighted guesses; path-guessed git slugs stop fabricating
    // `remote_host: "github.com"` and carry `slug_source`; PR outcomes carry the
    // commit + method they were read from; CJK/Cyrillic cognition tags survive;
    // segments carry `local_time`; `mcp.`/`mcp:` tool spellings split correctly
    // (their aggregate keys move). Cross-parser by construction, so the CAPTURE
    // aspect carries the whole wave and the parser aspects stay put — one fleet
    // re-scan, mostly served by the span cache and the cloud classifier.
    Semantics::Semantic,
    // v25 — segments state which SCAN produced them (core#701). The scan flushes
    // every `BATCH_MAX_EVENTS`, so a session's segmentation leaves in several
    // batches, and the server inferred what a batch superseded from TIME
    // OVERLAP. A cursor-resumed scan overlaps older segments without re-stating
    // them, so the server retired spans no batch ever restated: 116 sessions and
    // 50,651,192,068 tokens — 29.5% of all measured work — ended up with no live
    // segment at all, invisible to every taxonomy, insight and node-spend read
    // while the rollups kept counting the events. Batches now carry
    // `segment_generations`, and this bump is the repair: only a re-read
    // regenerates the segments that were retired without a replacement.
    // Cross-parser, because the loss is in supersession rather than in any one
    // parser — claude_code and pi were worst hit.
    Semantics::Semantic,
    // v26 — tool calls carry their end instants, paired from what the logs
    // state. A `ToolCallWire`'s `ended_at` is how the server splits a turn into
    // model thinking vs waiting on tools, and history uploaded by earlier builds
    // carries it almost nowhere — so tool wait read as ~0 everywhere. Each
    // source states the end in its own place and the parsers now read all of
    // them: Claude Code's `tool_result` line dates the call it answers (and an
    // UNDATED result line no longer stamps `ended_at: ""`, which parses as the
    // epoch); codex's `*_tool_call_output` records already dated their calls,
    // and its `event_msg`/`mcp_tool_call_end` — until now an unmodelled kind
    // that only warned — states a whole MCP call by itself (invocation, result,
    // end instant, measured duration), so those calls exist at all now; pi's
    // `toolResult` line already dated its call. Cursor's bubble store states
    // tool status but no end instant, so cursor honestly emits none (missing
    // means unknown; a fabricated end poisons the decomposition). Rides with it:
    // codex's `thread_goal_updated` and `turn_aborted` become modelled events
    // instead of warnings, and `inter_agent_communication_metadata` (one
    // undocumented boolean) is consumed as a decision instead of ledgered noise.
    // All of it is local-only fact: only a re-read fills the historical spans,
    // and the deterministic `tc_` ids mean the server upserts the ends onto the
    // calls it already has. Cross-parser, so the CAPTURE aspect carries it.
    Semantics::Semantic,
    // v27 — provenance tiering for repo slugs. Segments derive their `projects`
    // hint from stored events at scan time, and that derivation changed shape:
    // the hint now reads the first VERIFIED slug in the slice (not blindly the
    // first event), tiers its confidence by `slug_source` — with a real
    // `remote_url` accepted as evidence on pre-marker events, since no guess
    // path ever wrote one — and ships the marker verbatim as the hint's
    // `reason`; session-metadata repo refs ship `git` vs `git_guess` from the
    // same predicate. All of it is re-derived from the local logs, so a re-read
    // is the only way historical segments gain the tiering. Cross-parser (every
    // parser's events carry git context), so the CAPTURE aspect carries it.
    Semantics::Semantic,
    // v28 — a session's repository is a fact about the FILES it touched,
    //       not about where the agent happened to be started. Placement routes on
    //       the segment `projects` hint, that hint takes only a VERIFIED slug
    //       (`git_remote` or `repo_root_dir`), and the daemon derived the slug by
    //       resolving `cwd` and nothing else — so an agent launched from a
    //       directory that HOLDS checkouts rather than being one stated no repo at
    //       all, on every session, forever. Measured over all time: `pi` stated a
    //       repo on 0 of 327 sessions, `cursor` on 0 of 202, Claude Desktop on 0
    //       of 51, against `claude_code`'s 1,131 of 1,147 — the one agent normally
    //       launched from inside the checkout. Downstream that is not a missing
    //       label but a wrong home: 306 of one user's 317 sessions fell through to
    //       their router's personal workspace instead of the org that owns the
    //       code they were editing. Events now carry the paths their tool calls
    //       named — local-only, shed at the wire door beside `cwd`, because an
    //       absolute path is a home directory — and resolution tries the parent
    //       directory of each of them, most specific first, before falling back to
    //       `cwd`. CROSS-PARSER: claude_code, codex and pi all produce the paths,
    //       and every parser's events are re-filed by the corrected identity, so
    //       the CAPTURE aspect carries it rather than any parser aspect. A FLEET
    //       RE-SCAN is required and is the whole point — `git.slug_source` is the
    //       documented re-ship trigger (`consumer/src/backfill_segment_hints.rs`
    //       re-ships a session when it changes), and re-reading the local logs is
    //       the only thing that can re-file a session already uploaded: the server
    //       cannot derive a repo from data that never named one. Cursor is not a
    //       producer here — it parses no tool calls, so it has no paths to offer
    //       and its sessions still wait on a source of their own.
    Semantics::Semantic,
    // v29 — `~/…` tool paths name the home directory. The generation above
    //       resolved a repo from the parent of every path a tool call named,
    //       and that fixed nothing for the user it was measured on: the agent
    //       spells its paths the way the person typed them, `~/Documents/<repo>/…`,
    //       and `~` is neither root-stated nor a real relative segment, so the
    //       join produced `<cwd>/~/Documents/<repo>` — a directory nobody has.
    //       Measured on that user's sessions after the v28 re-scan: 207 tool-path
    //       events, 0 resolved; 79 of 80 sessions in a fortnight still in the
    //       personal workspace. `~/` now expands to the reading machine's home
    //       (the daemon reads transcripts on the machine that wrote them).
    //       Cross-parser for the same reason v28 was, and a re-read is again the
    //       only thing that can re-file a session already uploaded.
    Semantics::Semantic,
    // v30 — the resolution budget counts REPOSITORIES, not directories, and a
    //       named path is the candidate itself rather than its parent. The v29
    //       re-scan on the machine it was measured on still shipped 17 sessions
    //       that name a checkout on disk with no repo: a re-scan batches many
    //       sessions, every file's parent was its own budget slot, 64 were gone
    //       a few sessions in, and every event after that resolved nothing.
    //       Replayed as one batch over that machine's 278 pi sessions: 34
    //       resolved under the directory budget, 178 under the root budget
    //       (the other 100 name no path inside any checkout). A turn that only
    //       named a checkout's root directory walked up from its PARENT and
    //       found nothing; it now walks from the path. Cross-parser, and a
    //       re-read is again the only way a shipped session gains the repo.
    Semantics::Semantic,
];

/// `redaction` — the other cross-parser aspect: a bump re-reads EVERY parser's
/// files, because what leaves the box changed meaning.
const REDACTION: &[Semantics] = &[];

/// `claude_code` — a parser-scoped aspect: a bump re-reads only Claude Code's
/// transcripts.
const CLAUDE_CODE: &[Semantics] = &[
    // v24 — the durations Claude Code MEASURED, which only the local JSONL ever
    // held. It states its own elapsed time under the name each tool chose, with
    // the unit in that name (`durationMs`, `durationSeconds`, `totalDurationMs`
    // all ship in one release), and the parser read exactly one spelling — so a
    // web search and a sub-agent run, the two longest calls a session makes,
    // reported no duration at all. Also stops dating a turn that states no
    // instant to the epoch: such a line now reports through the skip ledger
    // instead of shipping `ts: ""`, which parses as 1970 and drags every wait
    // derived from it. Re-reading is the only way history gains the numbers;
    // nothing on the server can derive them.
    Semantics::Semantic,
    // v25 — the model's own THINKING, on transcripts already read. Claude Code
    // writes reasoning as `thinking` content blocks beside the `text` ones, and
    // the excerpt pass filtered to `text` — so every session that reasoned
    // shipped no trace of having reasoned, and the block's opaque `signature` is
    // the only part of it nothing wants. Turns now carry
    // `reasoning_excerpt`/`reasoning_bytes` (redacted on the same fail-closed
    // path as the prose; the signature stays behind). Re-reading is the only way
    // history gains it: the blocks exist solely in the local JSONL. The 1,694
    // sub-agent transcripts this release starts walking need no bump — they have
    // no cursors to wipe — but the SAME re-read is what makes existing main
    // transcripts give up their thinking.
    Semantics::Semantic,
    // v26 — a transcript record is identified by its own `uuid`.
    // `--resume`/`--continue` writes a new transcript that opens with copies of
    // the ancestor's records, and the parser recognised a copy by the one thing
    // it used to state: a `sessionId` that disagreed with the filename. Current
    // Claude Code REWRITES that field to the new file's own uuid, so a copy is
    // byte-identical to its original except for the single field the rule read —
    // and the rule stopped firing. Keyed on `(file, byte offset)`, every
    // replayed record then minted a fresh id: on one real machine 14,865 of
    // 413,066 emitted records were the same line read out of two transcripts,
    // across 32 files, each of them counted twice — 3.6% of the corpus, and
    // concentrated in whichever sessions the user resumed most, which are the
    // long ones. The `uuid` survives every copy shape byte for byte (verified:
    // the full record diff between a copy and its original is that one field),
    // so it is the key now, and a record that states no uuid still falls back to
    // its position. It also collects a second shape the positional key could
    // never see: 15 transcripts on that machine re-append a record they already
    // hold — same uuid, same `requestId`, same `message.id`, same counters —
    // 9,553 lines whose tokens were billed twice. The ancestor probe stays for
    // the OLD shape alone, deciding only whether a copy that still declares its
    // ancestor is worth emitting. Session identity is untouched — a transcript's
    // records belong to the session its filename names, as codex v28
    // established. A re-scan is what re-keys the history; the events the old key
    // landed are the server's to tombstone.
    Semantics::Semantic,
];

/// `codex` — a parser-scoped aspect: a bump re-reads only codex's rollouts.
const CODEX: &[Semantics] = &[
    // v24 — the turn ordinal and codex's own turn duration. `turn_index` counted
    // usage-bearing `token_count` lines, i.e. API round trips, so one typed
    // prompt whose reply took three round trips reported three turns and the
    // field meant something different for codex than for every other agent — a
    // cross-agent reading of turn timing cannot survive that. It now advances at
    // the typed prompt, as claude_code, pi and cursor already did. And
    // `task_complete` states `duration_ms`, the only number in a rollout that
    // says how long a turn took; the record has no parser arm, so the number was
    // dropped. A stated duration is structural, like the instant and the ids, so
    // unmodelled records carry it now. Both are local-only facts: a re-scan is
    // the only way uploaded sessions get them.
    Semantics::Semantic,
    // v25 — which files the work actually touched. `files_touched` had no
    // producer in ANY parser — every construction site was `Vec::new()`, and the
    // only non-empty occurrences in the tree were test fixtures — so the
    // taxonomy `components` dimension the server derives from it
    // (`components_from_slice`, one `hint("components", …, 0.6)` per value) has
    // been computed over an empty list on every session, from every agent, for
    // as long as the field has existed. Codex is the agent that STATES the
    // answer: `event_msg`/`patch_apply_end` keys `payload.changes` by the path
    // of each file it just edited (1,019 such records in one real session), and
    // the parser dropped the whole record as unmodelled. It now ships as an
    // event carrying those paths, made safe where they are read: relative to the
    // session's `cwd` when they sit under it, the file's name alone when they do
    // not, so no home directory ever leaves the machine. The unified diffs in
    // that payload stay on disk. Only the codex aspect moves — ~65 codex
    // sessions re-read, not the corpus.
    Semantics::Semantic,
    // v26 — a round trip's identity. A `codex resume`/subagent-spawn rollout
    // opens with its own `session_meta`, then REPLAYS the ancestor's whole
    // history — and codex rewrites every copied timestamp to the fork moment, so
    // a copy shares no position and no instant with its original. Keyed on
    // `(file, byte offset)`, each replay minted a brand-new event under the
    // ancestor's session id, and one 14-day conversation forked 448 times billed
    // itself 51x (466.9B of a 489.7B account). Claude Code and cursor were never
    // exposed: their copies keep the line uuid, so the store collapses them.
    // Codex gives a line no uuid, but `total_token_usage` — the conversation's
    // running total — survives a copy byte for byte, so it is the key now. A
    // re-scan is what re-keys the history; the server tombstones the events the
    // old key already landed. Rides the SAME codex bump wave as v25 where it can
    // — but v25 landed first, so this is its own re-read.
    Semantics::Semantic,
    // v27 — the multi-agent run, which was three unmodelled record types.
    // `event_msg`/`sub_agent_activity` is the sub-agent LIFECYCLE codex states
    // about itself (2,132 records in one real rollout: 446 starts, 1,649
    // interactions, 37 interruptions across 440 agents) and it dropped whole.
    // `response_item`/`agent_message` is the traffic BETWEEN those agents —
    // author, recipient, and what was said — and 3,109 of them shipped as
    // content-free unknown records; it has no `event_msg` twin, so capturing it
    // doubles nothing. `event_msg`/`agent_reasoning` is the model's thinking,
    // 14,802 records buffered onto the round trip's own usage-bearing event
    // exactly as its prose already was. All three are local-only facts, so a
    // re-scan is the only way uploaded sessions get them; only codex's own files
    // re-read.
    Semantics::Semantic,
    // v28 — a rollout file is its OWN session. A fork (`codex resume`, a subagent
    // spawn) opens with its own `session_meta` naming its own uuid, then REPLAYS
    // the ancestor's history — the ancestor's `session_meta` included — and the
    // parser took every declared id as identity. So each fork rebound to its
    // ancestor the moment the replay began, and its own new work landed there
    // too: on one real machine 447 of 485 rollout files collapsed into ONE
    // session holding 7,339,890 event rows spanning two weeks — 64% of the
    // entire events table. That session exceeds every processing ceiling, so it
    // yielded no tasks, no taxonomy and no attribution, while its 9,138 uploads
    // starved the summarise queue. The filename's uuid is the identity now; a
    // declared id that disagrees is an ancestor pointer, and the payload wins
    // only when the path names nobody. Round-trip KEYS are untouched — they
    // still hash the conversation the region declares, which is what makes a
    // replayed turn collapse onto the original (v26) rather than bill twice. A
    // re-scan is what re-keys the history to the sessions that actually did the
    // work: the events the old binding landed are the server's to tombstone.
    Semantics::Semantic,
    // v29 — the records a fork replays, keyed on what codex CALLS them. v26
    // keyed the token-bearing round trips and v25's `patch_apply_end` keyed on
    // `call_id`, but every other codex record still hashed `(file, byte
    // offset)` — and a fork replays the ancestor's whole history at a fresh
    // offset with a rewritten timestamp, so each copy minted a brand-new event.
    // On one real machine of 485 rollout files that is 454,155 of the 467,750
    // records now covered: the same work counted up to 348 times, worst in the
    // multi-agent runs, where 437,195 `sub_agent_activity` lines are 5,498 real
    // lifecycle events and 431,697 replays of them. Also 3,109 duplicate
    // inter-agent `agent_message` records, 6,350 duplicate typed prompts (6,491
    // records are 136 prompts, because every fork replays the root prompt), and
    // 13,000 duplicate `web_search_end` / `task_started` / `task_complete`
    // records. Codex states an id on these records and it survives a copy byte
    // for byte — the FIELD differs by record type (`call_id`, `client_id`,
    // `event_id`, `turn_id`, `id`, or `turn_id` nested in the passthrough
    // envelope), so one narrowest-first vocabulary is tried against every record
    // instead of a table of type→field. An id alone is not a record: it names a
    // CONTAINER, so the key is `(stated id, record type, ordinal under that id
    // in this file)` — the type because a turn's `task_started` and
    // `task_complete` state the same uuid, the ordinal because one turn carries
    // up to 157 inter-agent messages and a replay copies them in order. Verified
    // against the whole corpus: no key value covers two records that differ in
    // any field. NOT content-keyed, though a content hash was the obvious
    // candidate: codex rewrites a record between copies — an observed
    // `turn_aborted` ships once with `completed_at`/`duration_ms` and once
    // without — so hashing content splits one event in two, and it would also
    // fuse the same prompt genuinely typed twice. Records that state no id keep
    // their position, which is the honest answer rather than a lesser one:
    // `context_compacted` is the literal payload `{"type":"..."}` (40,101 of
    // them), and `thread_settings_applied` and `thread_goal_updated` name
    // nothing either. `turn_aborted` also stays positional — it is the one
    // record type whose content is known to vary between copies, so its key
    // needs evidence this corpus does not give. A re-scan is what re-keys the
    // history; the events the old key already landed are the server's to
    // tombstone.
    Semantics::Semantic,
    // v30 — a free-form custom tool input is not shell evidence. Codex writes
    // JavaScript and patch bodies as `custom_tool_call.input: String`; the
    // shared extractor accepted every raw string as a command, so these calls
    // became shell actions named after their first source token and entered the
    // command classifier. Shell calls state an object `command` or `cmd` field;
    // only that observed structure now selects the shell surface. The raw input
    // still hashes unchanged and the tool call still ships under its stated
    // name. Re-reading Codex history repairs the derived surface and removes the
    // false local script contexts; no other parser emits this custom string
    // record shape.
    Semantics::Semantic,
];

/// `cursor` — a parser-scoped aspect: a bump re-reads only Cursor's bubble
/// store.
const CURSOR: &[Semantics] = &[];

/// `pi` — a parser-scoped aspect: a bump re-reads only pi's transcripts.
const PI: &[Semantics] = &[
    // v24 — a record's identity is the id it states, not where it sat. Every
    // pi `message` / `model_change` line carries an `id` (an 8-hex node id in
    // the session's tree) and the parser keyed all of them by byte offset. pi
    // rewrites a transcript in place — a compaction, a re-titled header — and
    // every offset below the edit moves, so the same call shipped again under
    // a fresh `fs::` key and the server's `(scope, session, source_event_id)`
    // dedupe could not see it: 31,227 pi events landed twice on prod by
    // 2026-09-04. The key is now `rec::<session>::<id>` — session-scoped,
    // because the ids repeat across sessions — and a line stating no `id`
    // keeps its position. Semantic: the same bytes now produce different ids,
    // so history must be re-read; the re-shipped rows collide with their old
    // positional twins on the server, which retires the pair hourly
    // (core#1366).
    Semantics::Semantic,
];

/// Per-ASPECT pipeline generations — several exact claims instead of one
/// maximal one. Each aspect lists what it has declared SINCE
/// [`LEGACY_WORLD_VERSION`], oldest first, and the aspect's version is how many
/// there are ([`aspect_version`]).
///
///   · `capture` / `redaction` — cross-parser aspects; a Semantic bump re-reads
///     EVERY file (verbatim capture shape, redaction semantics).
///   · one aspect per parser — a parser-scoped fix (a codex token-counting bug,
///     a cursor schema move) re-reads only that parser's files.
///
/// The interface stays bounded (this fixed key set); the deleted structure is
/// the old implicit claim that any change invalidates the world.
///
/// **To bump: append ONE [`Semantics`] to an aspect and write the why above
/// it**, exactly as the v1–v23 history did. There is deliberately no number to
/// raise instead — the version is derived from this list, so a bump that states
/// no kind is not a thing the type system can express:
///
/// ```compile_fail
/// use modelstat_ingest::processing::Semantics;
/// // "codex v30", bumped the old way — a number, no stated kind. The slice is
/// // a slice of generations, so this does not build.
/// const CODEX: &[Semantics] = &[Semantics::Semantic, 30];
/// ```
pub const ASPECT_DERIVATIONS: &[(&str, &[Semantics])] = &[
    ("capture", CAPTURE),
    ("redaction", REDACTION),
    ("claude_code", CLAUDE_CODE),
    ("codex", CODEX),
    ("cursor", CURSOR),
    ("pi", PI),
];

/// An aspect's compiled version: the legacy base plus every generation it has
/// declared since. The number is OUTPUT — nothing anywhere writes one down.
#[must_use]
pub const fn aspect_version(generations: &[Semantics]) -> i64 {
    LEGACY_WORLD_VERSION + generations.len() as i64
}

/// Every aspect and its compiled version — what the stored state is reconciled
/// against.
pub fn aspect_versions() -> impl Iterator<Item = (&'static str, i64)> {
    ASPECT_DERIVATIONS
        .iter()
        .map(|(aspect, generations)| (*aspect, aspect_version(generations)))
}

/// Does the span this bump owes — `(stored, compiled]` — contain a
/// [`Semantics::Semantic`] generation, i.e. must history be re-read?
///
/// The whole owed SPAN, not just the newest generation. A device auto-updates
/// across however many releases it was away for, so it commonly arrives with
/// several generations owed at once, and one Semantic among them makes the
/// whole span a re-read. Taking only the latest would silently skip the repair
/// an earlier generation in the same span was published to make.
///
/// A `stored` below [`LEGACY_WORLD_VERSION`] owes a replay unconditionally. The
/// span then reaches back into the single-integer era, where a bump could only
/// make the maximal claim and no generation declared anything — so nothing here
/// can say those were shape-only. Silence is not a Mechanical declaration, and
/// the conservative direction is the one this whole mechanism takes elsewhere:
/// over-reading costs a re-read, under-reading skips a repair for good.
#[must_use]
pub fn replay_owed(generations: &[Semantics], stored: i64) -> bool {
    if stored < LEGACY_WORLD_VERSION {
        return true;
    }
    generations
        .iter()
        .skip((stored - LEGACY_WORLD_VERSION) as usize)
        .any(|s| *s == Semantics::Semantic)
}

/// The daemon's PROCESSING_VERSION — the ONE number `IngestBatch`'s
/// `processing_version` states, naming the generation of local processing that
/// cut a batch's segments and titles.
///
/// It is the legacy base plus every generation every aspect has declared, so it
/// reads on the same axis the retired single integer used and moves by exactly
/// one whenever any aspect bumps. That is the whole requirement the server puts
/// on it: a number that is DIFFERENT for two daemon builds whose processing
/// differs, so two re-ships of the same session stop tying on the server's row
/// version and being resolved by merge order.
///
/// Not a per-aspect number, because the batch is not per-aspect: one batch
/// carries whatever the scan read, and the server is asking "which generation
/// of you produced this", not "which part of you moved last". Not the maximum
/// of the aspects either — a bump to a lagging aspect would leave the maximum
/// where it was, and two differing builds would tie again, which is the exact
/// collision this exists to break.
pub const PROCESSING_VERSION: u32 = {
    let mut generations = 0usize;
    let mut i = 0;
    while i < ASPECT_DERIVATIONS.len() {
        generations += ASPECT_DERIVATIONS[i].1.len();
        i += 1;
    }
    LEGACY_WORLD_VERSION as u32 + generations as u32
};

// The server REFUSES a batch above this ceiling rather than clamping to it, so
// a daemon that could produce one would be permanently rejected — every batch
// 400'd, every cursor wedged. Proven here at compile time instead: the number is
// derived from a table this crate owns, so the only way to breach the ceiling is
// to declare 65,513 more generations, and that build does not exist.
const _: () = assert!(
    PROCESSING_VERSION <= modelstat_wire::caps::PROCESSING_VERSION_MAX,
    "PROCESSING_VERSION exceeds what the server can store — the batch would be refused, not clamped"
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The single-source invariant that makes the declaration compulsory: a
    /// version is nothing but a count of declared generations, so no number can
    /// disagree with the kinds beside it — there is no second place to write one.
    #[test]
    fn every_aspect_version_is_its_declared_generation_count() {
        for (aspect, generations) in ASPECT_DERIVATIONS {
            assert_eq!(
                aspect_version(generations),
                LEGACY_WORLD_VERSION + generations.len() as i64,
                "aspect {aspect} has a version from somewhere other than its declarations"
            );
        }
    }

    /// The wire number moves for EVERY bump of ANY aspect — the property the
    /// server's tie-break rests on. A maximum would not: bumping a lagging
    /// aspect leaves it unmoved.
    #[test]
    fn the_wire_version_moves_when_any_single_aspect_bumps() {
        let total = |table: &[(&str, &[Semantics])]| -> i64 {
            LEGACY_WORLD_VERSION + table.iter().map(|(_, g)| g.len() as i64).sum::<i64>()
        };
        assert_eq!(total(ASPECT_DERIVATIONS), i64::from(PROCESSING_VERSION));

        // The lagging aspect — the one a maximum would ignore — bumps.
        let bumped: Vec<(&str, &[Semantics])> = vec![
            ("capture", CAPTURE),
            ("redaction", &[Semantics::Mechanical]),
            ("claude_code", CLAUDE_CODE),
            ("codex", CODEX),
            ("cursor", CURSOR),
            ("pi", PI),
        ];
        assert_eq!(
            total(&bumped),
            i64::from(PROCESSING_VERSION) + 1,
            "a bump to the aspect furthest behind must still move the stated number"
        );
    }

    #[test]
    fn a_semantic_generation_in_the_owed_span_owes_a_replay() {
        let gens = [Semantics::Mechanical, Semantics::Semantic];
        assert!(
            replay_owed(&gens, LEGACY_WORLD_VERSION),
            "both generations owed, one of them Semantic"
        );
        assert!(
            replay_owed(&gens, LEGACY_WORLD_VERSION + 1),
            "the Semantic one is still owed"
        );
        assert!(
            !replay_owed(&gens, LEGACY_WORLD_VERSION + 2),
            "nothing owed at all"
        );
    }

    /// The reason the span is read rather than the newest generation alone: a
    /// device that skipped a release arrives owing several at once, and the
    /// Semantic one buried behind a Mechanical one still has to be honoured.
    #[test]
    fn a_mechanical_generation_hides_no_semantic_one_behind_it() {
        let gens = [Semantics::Semantic, Semantics::Mechanical];
        assert!(
            replay_owed(&gens, LEGACY_WORLD_VERSION),
            "the newest generation is Mechanical, but the skipped one is not"
        );
        assert!(!replay_owed(&[Semantics::Mechanical], LEGACY_WORLD_VERSION));
    }

    /// An install stored below the base owes a replay whatever the aspect has
    /// declared since — the span crosses the single-integer era, and those
    /// generations never stated a kind. Reading their silence as Mechanical
    /// would skip, forever, exactly the repairs that era's bumps were published
    /// to make.
    #[test]
    fn a_prehistoric_stored_version_owes_a_replay_whatever_was_declared_since() {
        assert!(replay_owed(&[Semantics::Semantic], 1));
        assert!(replay_owed(&[Semantics::Mechanical], 1));
        assert!(
            replay_owed(&[], LEGACY_WORLD_VERSION - 1),
            "an aspect that has declared nothing since the base still owes the era behind it"
        );
    }
}
