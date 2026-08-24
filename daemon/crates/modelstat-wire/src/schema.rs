//! Wire schemas — serde types bit-aligned with `packages/core/src/schemas.ts`
//! and the server's Rust ingest schema (feature §17.3).
//!
//! Conventions that keep this a faithful port of the Zod schemas:
//!   * A Zod `.nullable()` field is required-but-may-be-null: modeled as
//!     `Option<T>` that serializes `null` when `None` (present). `#[serde(default)]`
//!     makes deserialize tolerant of an absent key too.
//!   * A Zod `.optional()` field may be absent: modeled as `Option<T>` with
//!     `skip_serializing_if = "Option::is_none"`, so `None` is omitted (matching
//!     JS `JSON.stringify` dropping `undefined`).
//!   * A Zod `.default(x)` field always materializes on output; `#[serde(default)]`
//!     supplies `x` on input.
//!   * Additive nested blobs the daemon merely passes through (`references`,
//!     `session_metadata`, `session_installs`, heartbeat `stats`) are held as
//!     `serde_json::Value` for lossless round-trip; their full typed ports land
//!     with the enrichment milestone (M4). This is strictly more faithful for
//!     the wire-parity test than a hand re-serialization.
//!
//! Caps live in [`crate::caps`]; [`IngestBatch::clamp`] applies them in UTF-8
//! bytes (the explicit clamp layer, plan D12).

use crate::caps;
use crate::clamp::{clamp_in_place, clamp_opt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Token usage — every field defaults to 0 (Zod `.default(0)`), so all five
/// always serialize.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_creation: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub reasoning: u64,
}

/// Where [`GitContext::remote_slug`] came from. The daemon reaches a slug three
/// ways and they are not equally true: only [`SLUG_SOURCE_GIT_REMOTE`] read the
/// repo's own configured remote. Stamped so the server can weigh a fact against
/// a guess instead of receiving both as the same string.
pub const SLUG_SOURCE_GIT_REMOTE: &str = "git_remote";
/// The slug is the repo-ROOT directory name — a real repo on disk, but one with
/// no configured remote, so it names no forge and no owner.
pub const SLUG_SOURCE_REPO_ROOT_DIR: &str = "repo_root_dir";
/// The slug was inferred from the SHAPE of the cwd (`…/projects/<a>/<b>`) with
/// no repo reachable at all. A guess about a path, not an observation of git.
pub const SLUG_SOURCE_PATH_SHAPE: &str = "path_shape";

/// The `projects` hint confidence for a slug whose provenance is verified
/// ([`slug_is_verified`]) — a fact read off the repo itself.
pub const PROJECT_SLUG_CONFIDENCE_VERIFIED: f64 = 1.0;
/// The `projects` hint confidence for a surviving guess (path shape, or
/// unstated provenance with no remote evidence) — below the server's
/// project-node minting bar on purpose.
pub const PROJECT_SLUG_CONFIDENCE_GUESS: f64 = 0.5;

/// True when the context's slug was read off the repo itself. Two kinds of
/// evidence qualify:
///   - the `slug_source` marker states it (`git_remote` / `repo_root_dir`);
///   - the context carries a `remote_url` — NO guess path, current or
///     historical, has ever written one, so a pre-marker event with a remote
///     URL was read off git config even though it predates the marker.
///
/// False for a path-shape guess AND for an absent marker without a remote URL:
/// unstated provenance is not verification (SDK producers, and events from
/// daemons predating the marker).
pub fn slug_is_verified(git: &GitContext) -> bool {
    matches!(
        git.slug_source.as_deref(),
        Some(SLUG_SOURCE_GIT_REMOTE) | Some(SLUG_SOURCE_REPO_ROOT_DIR)
    ) || git.remote_url.is_some()
}

/// Git context — the four original fields nullable (present, may be null), plus
/// the additive `slug_source` provenance marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitContext {
    #[serde(default)]
    pub remote_url: Option<String>,
    /// The forge host, and ONLY when git itself named it (parsed out of
    /// `remote.origin.url`). Null is the honest value everywhere else — a slug
    /// that came from a path shape says nothing about where the repo is hosted.
    #[serde(default)]
    pub remote_host: Option<String>,
    #[serde(default)]
    pub remote_slug: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// One of the `SLUG_SOURCE_*` markers — how `remote_slug` was reached.
    /// Absent on contexts that carry no slug (and from daemons predating it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug_source: Option<String>,
}

/// Redaction report — three guaranteed counters plus `pf_*` catchall keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionReport {
    #[serde(default)]
    pub secrets_found: u64,
    #[serde(default)]
    pub emails_redacted: u64,
    #[serde(default)]
    pub paths_redacted_absolute: u64,
    /// `.catchall(z.number().int().nonnegative())` — extra `pf_<category>` keys.
    #[serde(flatten)]
    pub extra: BTreeMap<String, u64>,
}

/// One raw event — the canonical per-turn row (feature §17.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawEvent {
    pub source_event_id: String,
    /// This record's position in the source log it was read from, 1-based: the
    /// line ordinal in a transcript file, the record ordinal in a conversation
    /// for a key/value store. A stated observation of WHERE the record sat, not
    /// a claim about the session as a whole.
    ///
    /// It exists because `ts` cannot order a log: every parser sees runs of
    /// records sharing one millisecond, and some sources round to the second, so
    /// sorting by instant alone shuffles a conversation into an order nobody
    /// wrote. The source already answers this exactly — it is a list — and this
    /// is that answer, carried instead of thrown away and re-guessed downstream.
    ///
    /// Deterministic and stable across re-reads: every positional parser reads
    /// its file from the top on every scan (the upload cursor gates the SEND,
    /// never the READ), so the same record is the same ordinal forever, and the
    /// key/value parser counts in its own total order before any floor applies.
    /// Absent from producers with no source log to be positioned in — an SDK
    /// reports calls as they happen and has no list to count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub ts: String,
    /// When the work behind this event BEGAN, stated only by a producer that
    /// watched it begin. `ts` stays the event's own instant; this is a second
    /// fact about the same occurrence, never a re-reading of the first.
    ///
    /// The SDKs are the producers that can state it: they sit in the call path,
    /// so they hold the instant the request left and the instant it came back.
    /// A transcript parser is not — it reads a line that was written after the
    /// fact — so it omits this, which is what its absence means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// When the FIRST piece of the model's output arrived — time-to-first-token,
    /// as an instant rather than a duration so it can be read against the other
    /// two without knowing which clock produced it.
    ///
    /// Stated only when the producer actually observed a first chunk. A
    /// non-streaming call has no such moment to observe, and this is omitted
    /// there rather than filled with the completion instant: a latency read off
    /// a fabricated instant is worse than no latency at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token_at: Option<String>,
    pub kind: String,
    pub agent: String,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    pub session_id: String,
    /// WHICH agent-instance inside the session produced this event — the
    /// harness's OWN identifier for it, verbatim (codex states an `agent_path`
    /// like `/root/schema_review`; Claude Code states an `agentId` on every line
    /// of a sub-agent transcript). Absent means the session's ROOT actor, which
    /// is every event a single-agent harness ever emits.
    ///
    /// A pass-through string, never parsed: a path-shaped id is a tree to the
    /// harness that wrote it, and reading that tree is the server's job — the
    /// daemon would be committing to one harness's spelling of parenthood. The
    /// registry ([`IngestBatch::session_actors`]) carries what the harness
    /// stated ABOUT each id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Who this event was addressed TO, verbatim — set only on an event that IS
    /// a message from one agent-instance to another (codex's `agent_message`
    /// names an `author` and a `recipient`). Absent on everything else, which
    /// includes every message to or from the human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_actor_id: Option<String>,
    #[serde(default)]
    pub turn_index: Option<u64>,
    #[serde(default)]
    pub parent_event_id: Option<String>,
    /// The session's working directory, absolute. Local-only — [`Self::clamp`]
    /// clears it at the wire door, so it reaches the device's own readers (git
    /// resolution, session metadata, the tray label) and never the network.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub git: Option<GitContext>,
    #[serde(default)]
    pub tokens: Option<TokenUsage>,
    /// Counters the source stated that do not map onto [`TokenUsage`]'s five
    /// buckets, keyed by their path in the source object (`last_token_usage.
    /// input_tokens`) with the source's own names, verbatim.
    ///
    /// Absent in the ordinary case. It fills when an upstream moves its token
    /// schema: the parser then has numbers it cannot honestly bucket, and the
    /// choice is to keep them here or throw them away. Throwing them away is
    /// what "default the missing counter to 0" amounted to, and it cost a full
    /// fleet re-scan — the events were already priced wrong and the true numbers
    /// were gone. Kept, they can be re-bucketed later without re-reading anyone's
    /// disk.
    ///
    /// NUMBERS ONLY, and that is structural rather than a simplification: a
    /// drifted object is a shape nothing has validated, so only its numeric
    /// leaves are taken and any string in it is left behind — an unknown shape
    /// must never become a hole the redactor does not cover.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tokens_unmapped: BTreeMap<String, u64>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub tool_calls: BTreeMap<String, u64>,
    #[serde(default)]
    pub files_touched: Vec<String>,
    /// The filesystem paths this event's tool calls NAMED, verbatim as the
    /// source stated them — absolute where the agent wrote them absolute,
    /// cwd-relative where it wrote them relative. Local-only, exactly as
    /// [`Self::cwd`] is: [`Self::clamp`] clears it at the wire door.
    ///
    /// It exists because a session's REPO is a fact about the files the work
    /// touched, and [`Self::cwd`] is the least specific way to ask. An agent
    /// launched from a directory that HOLDS many checkouts but is not itself a
    /// checkout has a cwd that resolves to no repo at all, and so states no
    /// repo — forever, on every session, however much of a repo it edited.
    /// Measured over all time: `pi` stated a repo on 0 of 327 sessions,
    /// `cursor` on 0 of 202, Claude Desktop on 0 of 51, while `claude_code`
    /// — the one agent normally launched from inside the checkout — stated one
    /// on 1,131 of 1,147. The paths are what tell those sessions apart, and
    /// only THIS machine can resolve them: by the time a batch is on the
    /// network the disk they name is unreachable.
    ///
    /// Not [`Self::files_touched`], which answers the same question for the
    /// WIRE and is therefore made repo-relative or reduced to a bare filename
    /// at parse time — no redactor walks a `Vec<String>`, so that sanitising is
    /// the only thing keeping a home directory off the network. It is also what
    /// destroys the directories git resolution needs, so the two cannot be one
    /// field: this one keeps the whole path and never leaves the machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_excerpt: Option<String>,
    /// Chars of the cleaned message text BEFORE paste-elision/truncation
    /// (SPEC 0005) — "was this cut / how big was the real prompt" as a stored
    /// fact. Only set when `content_excerpt` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_bytes: Option<u64>,
    /// The model's REASONING for this turn, VERBATIM — the thinking it wrote
    /// before answering (Claude Code's `thinking` content blocks, codex's
    /// `agent_reasoning` records). Redacted exactly like
    /// [`Self::content_excerpt`], on the same fail-closed path: it is captured
    /// text, and there is no weaker treatment for it anywhere.
    ///
    /// A field of its own rather than more prose, because the two answer
    /// different questions — "what did it say" and "what was it working out" —
    /// and a reader that cannot tell them apart cannot ask either. Absent when
    /// the turn stated none, which is most turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_excerpt: Option<String>,
    /// Chars of the reasoning BEFORE the wire clamp — the `content_bytes`
    /// question for the other text: how big was it really. Only set when
    /// [`Self::reasoning_excerpt`] is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<Value>,
    #[serde(default)]
    pub source_file: Option<String>,
    #[serde(default)]
    pub source_byte_offset: Option<u64>,
    /// What the redactor REMOVED from this turn, counted by type — e.g.
    /// `{"secret": 1, "private_email": 2}`. Absent when nothing was removed.
    ///
    /// COUNTS ONLY, and that is a hard rule, not a simplification: never the
    /// values, and never a hash of them either. A hash would let anyone holding a
    /// guess confirm it, which recreates exactly the exposure redaction exists to
    /// prevent. The count says "a secret was here"; that is the whole feature.
    ///
    /// The markers in `content_excerpt` are the human-readable half of the same
    /// fact (`[REDACTED:SECRET]`) and this is the machine-readable half — one
    /// UPPERCASED key per marker, so the two can be checked against each other
    /// rather than trusted separately.
    ///
    /// Keys are the redactor's own category names (`modelstat_redact`'s
    /// `privacy_filter::CATEGORIES`) plus the deterministic floor's three
    /// (`secrets_found`, `emails_redacted`, `paths_redacted_absolute`). One
    /// vocabulary, defined by the model that produces it, so the stored dimension
    /// cannot drift from what the daemon actually detected.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub redactions: BTreeMap<String, u32>,
}

impl RawEvent {
    /// The wire's last gate, run on every event at the one door that reaches
    /// the network: bound every string to its cap (feature §17.2/§17.3), and
    /// shed the local filesystem paths that were never the server's to hold.
    pub fn clamp(&mut self) {
        clamp_opt(&mut self.model, caps::MODEL_MAX);
        clamp_in_place(&mut self.session_id, caps::SESSION_ID_MAX);
        for f in &mut self.files_touched {
            clamp_in_place(f, caps::FILES_TOUCHED_ITEM_MAX);
        }
        self.files_touched.truncate(caps::FILES_TOUCHED_COUNT_MAX);
        clamp_opt(&mut self.content_excerpt, caps::CONTENT_EXCERPT_MAX);
        clamp_opt(&mut self.reasoning_excerpt, caps::REASONING_EXCERPT_MAX);
        clamp_opt(&mut self.started_at, caps::EVENT_INSTANT_MAX);
        clamp_opt(&mut self.first_token_at, caps::EVENT_INSTANT_MAX);
        self.shed_local_paths();
        clamp_opt(&mut self.source_file, caps::SOURCE_FILE_MAX);
    }

    /// Forget where a file lived on the machine that produced it.
    ///
    /// Three fields hold an absolute path on the way out, past a floor that
    /// redacts exactly that shape out of every piece of TEXT: a command
    /// mentioning `/Users/<name>/…` shipped scrubbed while the `cwd` beside it
    /// shipped whole, and a Claude transcript's own directory name spells the
    /// project path out in full. What the server actually does with them
    /// decides what may stay, and every answer was read off its source:
    ///
    ///   * `cwd` — NOTHING. No column stores it, and the single matcher that
    ///     names it is set by nothing. Every real reader is on THIS machine
    ///     (git resolution, session metadata, the tray's label) and has long
    ///     since run by the time a batch reaches this method.
    ///   * `tool_paths` — NOTHING, for the `cwd` reason and then some: it IS
    ///     the cwd question asked of the files a turn named, its one reader
    ///     (git resolution) runs on this machine in the same scan that parsed
    ///     the event, and no wire schema anywhere has ever been told the field
    ///     exists.
    ///   * `source_file` — only its FILE NAME. The one consumer splits on the
    ///     separators and keeps the last component, to recognise a transcript
    ///     named after the session it holds. Every directory above that name
    ///     is the part that leaks and the part nothing reads.
    ///
    /// [`RawEvent::actor_id`] is deliberately NOT in that list even though it
    /// can read as a path (`/root/schema_review`): it names a position in the
    /// harness's own agent tree, not a location on this disk, and shortening it
    /// would destroy the only thing it is for.
    ///
    /// So the wire keeps the name and forgets the path. Enforced HERE rather
    /// than in each parser because this is the single door every batch passes
    /// through on its way out ([`crate`] callers reach it via `clamp`): a
    /// scrub each parser has to remember is a scrub that leaks the day
    /// somebody adds a parser.
    fn shed_local_paths(&mut self) {
        self.cwd = None;
        self.tool_paths.clear();
        if let Some(path) = self.source_file.as_deref() {
            // `rsplit` over both separators: a transcript written on Windows
            // names itself with `\`, and the server splits on both too.
            let name = path.rsplit(['/', '\\']).next().unwrap_or(path).to_string();
            self.source_file = Some(name);
        }
    }

    /// Collect into `out` — deduped, first-seen order — the filesystem paths
    /// ONE tool call's arguments name. The admission rule for
    /// [`Self::tool_paths`], written once because every parser that reads tool
    /// arguments needs the same answer and a rule each parser re-derives is a
    /// rule that drifts.
    ///
    /// Every string leaf of the argument object is considered, at any depth and
    /// under any key. Keying on argument NAMES (`file_path`, `path`,
    /// `notebook_path`, …) would be a claim about one harness's current tool
    /// schema, and the next tool to call its argument `target` would state no
    /// repo again — the very failure this field exists to end. What a path
    /// actually needs in order to name a directory is a separator, so that is
    /// the test, minus the two shapes that carry one while naming no local
    /// file: a URL, and a whole command line.
    ///
    /// Accumulating rather than returning, so a turn's several calls dedupe
    /// against each other without every caller repeating the bookkeeping.
    pub fn collect_tool_paths(args: &Value, out: &mut Vec<String>) {
        match args {
            Value::String(s) => {
                let path = s.trim();
                if is_tool_path(path) && !out.iter().any(|p| p == path) {
                    out.push(path.to_string());
                }
            }
            Value::Array(items) => {
                for v in items {
                    Self::collect_tool_paths(v, out);
                }
            }
            Value::Object(map) => {
                for v in map.values() {
                    Self::collect_tool_paths(v, out);
                }
            }
            // A number, bool or null names no file.
            _ => {}
        }
    }
}

/// Does `s` read as ONE path to something on this filesystem?
///
/// A separator is what makes a path able to name a directory at all, so it is
/// the requirement; `std::path::is_separator` asks the question for the machine
/// the log was written on, which is the only machine that will ever resolve it
/// (Windows counts `\`, POSIX does not). The two exclusions are the shapes that
/// carry a separator and still name no local file: a URL
/// (`https://host/org/repo`) and a command line (`git -C sub/dir status`),
/// which is several tokens rather than one path.
fn is_tool_path(s: &str) -> bool {
    !s.contains("://") && !s.contains(char::is_whitespace) && s.chars().any(std::path::is_separator)
}

/// A daemon-emitted taxonomy tag hint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaxonomyHintRooted {
    pub root_key: String,
    pub name: String,
    #[serde(default = "default_tag_confidence")]
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn default_tag_confidence() -> f64 {
    0.7
}

/// Privacy-preserving per-segment behavioral signal (counts only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentBehavior {
    #[serde(default)]
    pub user_turns: u64,
    #[serde(default)]
    pub correction_count: u64,
    /// A 0-1 score the daemon NO LONGER PRODUCES — it is omitted, not zeroed,
    /// so "no opinion" is distinguishable from "calm". The field survives for
    /// the payloads already stored and for the server that still reads it; the
    /// scoring belongs where it can be revised without a fleet release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frustration: Option<f64>,
}

/// The daemon machine's LOCAL wall clock at a segment's start.
///
/// The one fact only the device holds: every timestamp on the wire is UTC, so
/// once a segment leaves the box nothing can recover what time it was for the
/// person doing the work. The daemon used to spend that fact on two buckets
/// (`Morning`/`Night`, `Weekend`/`Friday`/`Weekday`) and throw the reading
/// away — a cut the server could never revise, redo per-org, or use for
/// anything else. These are the reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentLocalTime {
    /// Minutes east of UTC at that instant (`-420` for UTC-7), DST included —
    /// the zone as it actually was, not as the zone database reads today.
    pub utc_offset_minutes: i32,
    /// Local hour, 0-23.
    pub hour: u8,
    /// Local day of week, 0=Sunday … 6=Saturday (JS `getDay()`).
    pub weekday: u8,
}

/// A daemon-emitted segment — the sync unit (feature §17.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub segment_id: String,
    pub session_id: String,
    pub agent: String,
    pub started_at: String,
    pub ended_at: String,
    /// `abstract` is a Rust keyword — the raw identifier serializes as
    /// `"abstract"` on the wire (serde strips the `r#`).
    pub r#abstract: String,
    pub tokens: TokenUsage,
    #[serde(default)]
    pub tags: Vec<TaxonomyHintRooted>,
    pub redaction: RedactionReport,
    pub source_event_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abstract_embedding: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<SegmentBehavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_intent: Option<String>,
    /// The local wall clock at `started_at` — see [`SegmentLocalTime`]. Absent
    /// when the instant is unreadable, and from daemons predating it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_time: Option<SegmentLocalTime>,
}

impl Segment {
    pub fn clamp(&mut self) {
        clamp_in_place(&mut self.segment_id, caps::SEGMENT_ID_MAX);
        clamp_in_place(&mut self.session_id, caps::SESSION_ID_MAX);
        clamp_in_place(&mut self.r#abstract, caps::ABSTRACT_MAX);
        for t in &mut self.tags {
            clamp_in_place(&mut t.root_key, caps::TAG_ROOT_KEY_MAX);
            clamp_in_place(&mut t.name, caps::TAG_NAME_MAX);
            clamp_opt(&mut t.reason, caps::TAG_REASON_MAX);
        }
        self.tags.truncate(caps::TAGS_COUNT_MAX);
        self.source_event_ids
            .truncate(caps::SOURCE_EVENT_IDS_COUNT_MAX);
        clamp_opt(&mut self.user_intent, caps::USER_INTENT_MAX);
    }
}

/// One script summary inside a [`ToolAction`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptSummary {
    pub token: String,
    pub summary: String,
}

/// On-device action decomposition of a tool call — strict (unknown keys
/// rejected, mirroring Zod `.strict()`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolAction {
    pub surface: String,
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub qualifiers: Vec<String>,
    #[serde(default)]
    pub param_shape: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub r#abstract: Option<String>,
    #[serde(default)]
    pub command_redacted: Option<String>,
    #[serde(default)]
    pub scripts: Vec<ScriptSummary>,
    #[serde(default)]
    pub confidence: f64,
    pub extractor: String,
}

impl ToolAction {
    pub fn clamp(&mut self) {
        clamp_in_place(&mut self.surface, caps::TA_SURFACE_MAX);
        clamp_opt(&mut self.executable, caps::TA_EXECUTABLE_MAX);
        clamp_opt(&mut self.action, caps::TA_ACTION_MAX);
        clamp_opt(&mut self.object, caps::TA_OBJECT_MAX);
        for q in &mut self.qualifiers {
            clamp_in_place(q, caps::TA_QUALIFIER_ITEM_MAX);
        }
        self.qualifiers.truncate(caps::TA_QUALIFIERS_COUNT_MAX);
        clamp_opt(&mut self.param_shape, caps::TA_PARAM_SHAPE_MAX);
        for k in &mut self.keywords {
            clamp_in_place(k, caps::TA_KEYWORD_ITEM_MAX);
        }
        self.keywords.truncate(caps::TA_KEYWORDS_COUNT_MAX);
        clamp_opt(&mut self.r#abstract, caps::TA_ABSTRACT_MAX);
        clamp_opt(&mut self.command_redacted, caps::TA_COMMAND_REDACTED_MAX);
        for s in &mut self.scripts {
            clamp_in_place(&mut s.token, caps::TA_SCRIPT_TOKEN_MAX);
            clamp_in_place(&mut s.summary, caps::TA_SCRIPT_SUMMARY_MAX);
        }
        self.scripts.truncate(caps::TA_SCRIPTS_COUNT_MAX);
        clamp_in_place(&mut self.extractor, caps::TA_EXTRACTOR_MAX);
    }
}

/// One tool invocation (feature §17.3, §7.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallWire {
    pub external_call_id: String,
    pub session_id: String,
    pub source_event_id: String,
    #[serde(default)]
    pub segment_id: Option<String>,
    pub agent: String,
    pub server: String,
    pub name: String,
    #[serde(default)]
    pub turn_index: Option<u64>,
    pub call_index: u64,
    pub started_at: String,
    #[serde(default)]
    pub ended_at: Option<String>,
    pub status: String,
    pub args_hash: String,
    pub signature_hash: String,
    pub args_bytes: u64,
    pub result_bytes: u64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub action: Option<ToolAction>,
}

impl ToolCallWire {
    pub fn clamp(&mut self) {
        clamp_in_place(&mut self.external_call_id, caps::EXTERNAL_CALL_ID_MAX);
        clamp_in_place(&mut self.session_id, caps::SESSION_ID_MAX);
        clamp_opt(&mut self.segment_id, caps::SEGMENT_ID_MAX);
        clamp_in_place(&mut self.server, caps::SERVER_MAX);
        clamp_in_place(&mut self.name, caps::NAME_MAX);
        clamp_in_place(&mut self.args_hash, caps::ARGS_HASH_MAX);
        clamp_in_place(&mut self.signature_hash, caps::SIGNATURE_HASH_MAX);
        clamp_opt(&mut self.model, caps::MODEL_MAX);
        if let Some(a) = &mut self.action {
            a.clamp();
        }
    }
}

/// One merged PR mined from a repo's git history — an anchor point the server
/// compares AI-era outcomes against (the ROI denominator). Public repo facts
/// only (numbers, shas, timestamps, line counts) — the same safety class as a
/// slug; no file contents, no commit messages, no author identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorPr {
    pub pr_number: u64,
    /// Hex sha of the merge commit (7..=64 chars).
    pub merge_sha: String,
    /// ISO-8601 merge timestamp.
    pub merged_at: String,
    pub files_changed: u32,
    pub lines_added: u64,
    pub lines_deleted: u64,
    /// First-commit→merge wall time. Omitted when the history doesn't say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_ms: Option<u64>,
    /// Commits behind the merge. Omitted when the history doesn't say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_count: Option<u32>,
    /// Minutes of ACTIVE work behind the PR, from clustering its own commit
    /// timestamps into sittings. The effort half of the pair `span_ms` opens:
    /// wall time includes the night the PR spent in review, this does not.
    /// Omitted when the PR left fewer than two timestamps to cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_minutes: Option<u32>,
    /// Whether ANY commit in the PR carried an AI tool's trailer. Always false
    /// inside [`RepoAnchors::anchors`] — that list IS the human baseline — and
    /// carried explicitly so a consumer never has to infer it from context.
    #[serde(default)]
    pub ai_assisted: bool,
}

/// A repo's human-authored baseline: merged-PR shape stats mined once from its
/// own history. `head_sha` + `mined_at` pin WHAT was read and WHEN, so the
/// server can dedupe re-mines instead of averaging them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoAnchors {
    /// `org/repo`. The join key to `session_metadata` references.
    pub slug: String,
    /// The forge host, and only when git itself named it. Null everywhere else.
    #[serde(default)]
    pub host: Option<String>,
    /// ISO-8601 end of an OPERATOR-set mining window. Null by default: which
    /// PRs are a baseline is decided by AI trailers, not by a date.
    #[serde(default)]
    pub cutoff: Option<String>,
    /// ISO-8601 instant the mining ran.
    pub mined_at: String,
    /// Hex sha of the repo's HEAD at mining time (7..=64 chars).
    pub head_sha: String,
    /// Human-authored merged PRs found in the window scanned.
    #[serde(default)]
    pub human_anchor_count: u32,
    /// AI-assisted merged PRs found in that SAME window and excluded from
    /// `anchors`. Read next to `human_anchor_count` it gives the repo's
    /// AI-vs-human split a denominator — a fact the anchor list alone cannot
    /// carry. It is not a calibration signal; nothing downstream scores effort.
    #[serde(default)]
    pub ai_pr_count: u32,
    #[serde(default)]
    pub anchors: Vec<AnchorPr>,
}

impl RepoAnchors {
    pub fn clamp(&mut self) {
        clamp_in_place(&mut self.slug, caps::ANCHOR_SLUG_MAX);
        clamp_opt(&mut self.host, caps::ANCHOR_HOST_MAX);
        clamp_opt(&mut self.cutoff, caps::ANCHOR_ISO_MAX);
        clamp_in_place(&mut self.mined_at, caps::ANCHOR_ISO_MAX);
        clamp_in_place(&mut self.head_sha, caps::ANCHOR_SHA_MAX);
        for a in &mut self.anchors {
            clamp_in_place(&mut a.merge_sha, caps::ANCHOR_SHA_MAX);
            clamp_in_place(&mut a.merged_at, caps::ANCHOR_ISO_MAX);
        }
        self.anchors.truncate(caps::ANCHORS_PER_REPO_COUNT_MAX);
    }
}

/// The batch the daemon ships to `/v1/ingest` (feature §17.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestBatch {
    pub batch_id: String,
    pub device_id: String,
    pub daemon_version: String,
    pub events: Vec<RawEvent>,
    #[serde(default)]
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_installs: Option<Value>,
    /// The session's ACTOR REGISTRY — `session_id -> [actor, …]`, one entry per
    /// agent-instance the harness said it ran, so an `actor_id` on an event has
    /// something to join against. Held as a pass-through blob for the same
    /// reason `session_installs` is.
    ///
    /// Every actor is an object of VERBATIM STATED FACTS and every key is
    /// present ONLY when the harness stated it — an absent key means the
    /// harness said nothing, never a default:
    ///
    ///   * `id` — the only required key; matches the events' `actor_id`.
    ///   * `label` — what the harness CALLS this agent (Claude Code's
    ///     `agentType`: `Explore`, `general-purpose`, …).
    ///   * `description` — what the CALLER asked this agent to do, verbatim
    ///     (Claude Code's `description`). Prompt-derived text, so it arrives
    ///     floor-redacted like every other captured string.
    ///   * `path` — the harness's own path for it inside its agent tree
    ///     (codex's `agent_path`). Logical, never a filesystem path.
    ///   * `thread_id` — the harness's separate id for the conversation this
    ///     agent ran in (codex's `agent_thread_id`).
    ///   * `parent_actor_id` — the actor that spawned it, when the harness NAMES
    ///     it (Claude Code's `parentAgentId`). Never inferred from a path shape.
    ///   * `spawn_tool_use_id` — the tool call that spawned it (Claude Code's
    ///     `toolUseId`), which is what links an actor to the turn that asked
    ///     for it.
    ///   * `spawn_depth` — how deep the harness says it sits (Claude Code's
    ///     `spawnDepth`).
    ///   * `first_ts` / `last_ts` — the first and last instants this scan saw
    ///     the actor act. Derived from the actor's own events, so they are
    ///     observations of this file rather than claims about its lifetime.
    ///
    /// Additive: absent from a batch whose harness has no concept of more than
    /// one agent, which is most of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_actors: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_titles: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarizer_mode: Option<String>,
    /// The device's IANA zone name (`Europe/Berlin`), verbatim from the OS at
    /// the moment the batch was built — the durable fact behind
    /// [`Self::tz_offset_minutes`].
    ///
    /// Every instant on the wire is UTC, so once a batch leaves the box nothing
    /// can recover what time of day the work happened for the person doing it —
    /// and a device moves: a laptop crosses zones, a zone changes its rules.
    /// Stamped per batch rather than looked up per device, so each batch answers
    /// for itself instead of inheriting whatever the device last said.
    ///
    /// Verbatim means never checked against a roster of zones this build has
    /// heard of: the zone database gains and moves entries, and a name we
    /// cannot place is still the truthful answer to what the machine is set
    /// to. Absent when the OS states none — no zone is not UTC, and an absent
    /// name is never guessed from the offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    /// Minutes east of UTC in force when the batch was built (`-420` for
    /// UTC-7), DST included — stated beside [`Self::tz`] because the offset
    /// cannot reconstruct the zone and the zone alone cannot date a past
    /// instant without a tz database the reader may not have.
    ///
    /// The one spelling of this fact on the batch, and `i16` mirrors the
    /// server's declaration of it, so a reading the server must reject is
    /// unrepresentable here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz_offset_minutes: Option<i16>,
    /// Where this batch was REDACTED — `local` (on the device, the default and the
    /// only mode where nothing unscrubbed can leave), `cloud`, or `self-hosted`.
    /// Separate from `summarizer_mode` because the two are separate questions, and
    /// recorded per batch so "was this scrubbed on the box?" is answerable later
    /// rather than inferred from a setting that may since have changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redactor_mode: Option<String>,
    /// Pre-AI repo baseline anchors — one [`RepoAnchors`] per repo, mined
    /// on-device from the repo's own git history before its AI-era cutoff.
    /// Additive — old daemons omit it, old servers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_anchors: Option<Vec<RepoAnchors>>,
    /// What this batch's segments REPLACE, per session (core#701).
    ///
    /// The scan flushes every `BATCH_MAX_EVENTS`, so one session's segmentation
    /// leaves here split across batches. The server used to infer what a batch
    /// superseded by TIME OVERLAP, and a cursor-resumed scan overlaps older
    /// segments without re-stating them — so it retired spans no batch ever
    /// restated. 116 sessions and 50.65B tokens (29.5% of all measured work)
    /// ended up with no live segment at all.
    ///
    /// Only this side knows whether the file was re-read from the top or
    /// resumed at a cursor, so only this side can say what a batch restates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_generations: Option<BTreeMap<String, SegmentGeneration>>,
}

/// One session's claim about the segments in this batch (core#701).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentGeneration {
    /// Identifies the SCAN that produced these segments — identical across every
    /// batch of one scan, so the server can retire the generation this one
    /// replaces without ever retiring its own siblings.
    pub id: String,
    /// The span this generation supersedes, stated ONLY when the scan re-read
    /// the session's transcript whole. A scan resumed at a cursor APPENDS: it
    /// claims no span and the server retires nothing, because the segments it
    /// did not re-read are still the truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_to: Option<String>,
}

impl IngestBatch {
    /// Apply every UTF-8-byte cap across the whole batch (the wire-boundary
    /// clamp, feature §17.2). Array cardinality caps are enforced too so no
    /// permanently-rejectable batch can be produced.
    pub fn clamp(&mut self) {
        clamp_in_place(&mut self.daemon_version, caps::DAEMON_VERSION_MAX);
        clamp_opt(&mut self.tz, caps::TIMEZONE_MAX);
        for e in &mut self.events {
            e.clamp();
        }
        for s in &mut self.segments {
            s.clamp();
        }
        for t in &mut self.tool_calls {
            t.clamp();
        }
        if let Some(titles) = &mut self.session_titles {
            for v in titles.values_mut() {
                clamp_in_place(v, caps::SESSION_TITLE_MAX);
            }
        }
        self.events.truncate(caps::EVENTS_COUNT_MAX);
        self.segments.truncate(caps::SEGMENTS_COUNT_MAX);
        self.tool_calls.truncate(caps::TOOL_CALLS_COUNT_MAX);
        if let Some(repo_anchors) = &mut self.repo_anchors {
            for r in repo_anchors.iter_mut() {
                r.clamp();
            }
            repo_anchors.truncate(caps::REPO_ANCHORS_COUNT_MAX);
        }
    }
}

/// Heartbeat payload (feature §6.4/§17.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    pub device_id: String,
    pub status: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub progress_done: u64,
    #[serde(default)]
    pub progress_total: u64,
    #[serde(default)]
    pub queue_size: u64,
    #[serde(default)]
    pub stats: Value,
    #[serde(default)]
    pub last_event_at: Option<String>,
    pub daemon_version: String,
    /// The device's IANA time-zone name (`Europe/Berlin`), verbatim from the OS.
    ///
    /// The zone is the durable fact and the offset is only its reading at one
    /// instant — two devices at `+120` can be in different zones, and the same
    /// device reads `+60` six months later. Sent as the string the OS states,
    /// never mapped onto a roster of zones this build has heard of: the zone
    /// database gains and moves entries, and a name we cannot place is still the
    /// truthful answer to what the machine is set to.
    ///
    /// Absent when the OS will not say, which is honest: no zone is not UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Minutes east of UTC on this device right now (`-420` for UTC-7), DST
    /// included — the zone as it is in force, not as its rules read on paper.
    /// Sent beside [`Self::timezone`] rather than derived from it, so a reader
    /// never has to carry a zone database to know what time it is there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utc_offset_minutes: Option<i32>,
}

impl HeartbeatPayload {
    pub fn clamp(&mut self) {
        clamp_opt(&mut self.message, caps::HEARTBEAT_MESSAGE_MAX);
        clamp_in_place(&mut self.daemon_version, caps::DAEMON_VERSION_MAX);
        clamp_opt(&mut self.timezone, caps::TIMEZONE_MAX);
    }
}

/// The device fingerprint shared by register + heartbeat (feature §4). The
/// `machine_id` is the server's dedupe anchor and must be byte-identical
/// everywhere it is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub hostname: String,
    pub os_family: String,
    pub os_version: String,
    pub arch: String,
    pub daemon: String,
    pub daemon_version: String,
    pub machine_id: String,
}

/// Register-door body — `POST /v1/tokens` (feature §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub device_uuid: String,
    pub fingerprint: Fingerprint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenusage_defaults_all_zero_and_roundtrips() {
        let tu: TokenUsage = serde_json::from_str("{}").unwrap();
        assert_eq!(tu, TokenUsage::default());
        let json = serde_json::to_string(&tu).unwrap();
        assert!(json.contains("\"reasoning\":0"));
    }

    #[test]
    fn slug_verification_is_evidence_based() {
        let git = |slug_source: Option<&str>, remote_url: Option<&str>| GitContext {
            remote_url: remote_url.map(str::to_string),
            remote_host: None,
            remote_slug: Some("acme/web".into()),
            branch: None,
            slug_source: slug_source.map(str::to_string),
        };
        // The marker states it.
        assert!(slug_is_verified(&git(Some(SLUG_SOURCE_GIT_REMOTE), None)));
        assert!(slug_is_verified(&git(
            Some(SLUG_SOURCE_REPO_ROOT_DIR),
            None
        )));
        // A pre-marker event carrying a real remote URL: no guess path, current
        // or historical, ever wrote one, so the URL itself is the evidence.
        assert!(slug_is_verified(&git(
            None,
            Some("https://github.com/acme/web.git")
        )));
        // A guess stays a guess, marker or no marker.
        assert!(!slug_is_verified(&git(Some(SLUG_SOURCE_PATH_SHAPE), None)));
        assert!(!slug_is_verified(&git(None, None)));
    }

    #[test]
    fn tool_action_is_strict() {
        // Unknown key must be rejected (Zod `.strict()` parity).
        let err = serde_json::from_str::<ToolAction>(
            r#"{"surface":"shell","extractor":"shell.v3","bogus":1}"#,
        );
        assert!(err.is_err());
    }

    /// A synthetic home path, in the two spellings a transcript can carry.
    fn local_event(cwd: &str, source_file: &str) -> RawEvent {
        RawEvent {
            seq: None,
            started_at: None,
            first_token_at: None,
            source_event_id: "evt_x".into(),
            ts: "2026-01-01T00:00:00Z".into(),
            kind: "user_message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s".into(),
            actor_id: None,
            recipient_actor_id: None,
            turn_index: None,
            parent_event_id: None,
            cwd: Some(cwd.into()),
            git: None,
            tokens: None,
            tokens_unmapped: BTreeMap::new(),
            duration_ms: None,
            tool_calls: BTreeMap::new(),
            files_touched: vec![],
            // A path a tool call named, in the shape git resolution needs it:
            // whole, absolute, and therefore a home directory.
            tool_paths: vec![format!("{cwd}/src/main.rs")],
            content_excerpt: None,
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            references: None,
            source_file: Some(source_file.into()),
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    #[test]
    fn the_wire_keeps_the_file_name_and_forgets_the_path() {
        // POSIX and Windows spellings both reduce to the bare name, and the
        // cwd — which nothing server-side reads — does not survive at all.
        for (cwd, file, want) in [
            (
                "/Users/testuser/Projects/secret-client",
                "/Users/testuser/.claude/projects/-Users-testuser-Projects-secret-client/abc.jsonl",
                "abc.jsonl",
            ),
            (
                "C:\\Users\\testuser\\Projects\\secret-client",
                "C:\\Users\\testuser\\.codex\\sessions\\rollout.jsonl",
                "rollout.jsonl",
            ),
        ] {
            let mut ev = local_event(cwd, file);
            ev.clamp();
            assert_eq!(ev.cwd, None, "cwd must not reach the wire");
            assert_eq!(
                ev.tool_paths,
                Vec::<String>::new(),
                "tool_paths must not reach the wire"
            );
            assert_eq!(ev.source_file.as_deref(), Some(want));
        }
    }

    /// The server recognises a transcript named after the session it holds by
    /// splitting the path and reading the last component. Shedding the
    /// directories has to leave that reading intact — it is the only thing
    /// anything server-side does with this field.
    #[test]
    fn the_session_named_filename_still_reads_after_shedding() {
        let uuid = "0b3f1e2a-4c5d-6e7f-8a9b-0c1d2e3f4a5b";
        let mut ev = local_event(
            "/Users/testuser/x",
            &format!("/Users/testuser/.claude/projects/-Users-testuser-x/{uuid}.jsonl"),
        );
        ev.clamp();
        let shipped = ev.source_file.unwrap();
        let stem = shipped
            .rsplit(['/', '\\'])
            .next()
            .unwrap()
            .strip_suffix(".jsonl")
            .unwrap();
        assert_eq!(
            stem, uuid,
            "the server's filename→session read must survive"
        );
    }

    /// The invariant, stated over a whole batch rather than a field: nothing a
    /// device uploads may carry the absolute path of anything on it.
    #[test]
    fn no_absolute_local_path_survives_a_batch_clamp() {
        let mut batch = IngestBatch {
            batch_id: "b1".into(),
            device_id: "dev_1".into(),
            daemon_version: "daemon-test".into(),
            events: vec![local_event(
                "/Users/testuser/Projects/secret-client",
                "/Users/testuser/.codex/sessions/2026/07/22/rollout.jsonl",
            )],
            segments: vec![],
            tool_calls: vec![],
            session_installs: None,
            session_actors: None,
            session_titles: None,
            session_metadata: None,
            summarizer_mode: None,
            tz: None,
            tz_offset_minutes: None,
            redactor_mode: None,
            repo_anchors: None,
            segment_generations: None,
        };
        batch.clamp();
        let json = serde_json::to_string(&batch).unwrap();
        assert!(
            !json.contains("/Users/") && !json.contains("testuser"),
            "a local path reached the wire: {json}"
        );
    }

    #[test]
    fn nullable_serializes_null_optional_omits() {
        let ev = RawEvent {
            seq: None,
            started_at: None,
            first_token_at: None,
            source_event_id: "evt_x".into(),
            ts: "2026-01-01T00:00:00Z".into(),
            kind: "user_message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s".into(),
            actor_id: None,
            recipient_actor_id: None,
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: None,
            tokens_unmapped: BTreeMap::new(),
            duration_ms: None,
            tool_calls: BTreeMap::new(),
            files_touched: vec![],
            tool_paths: vec![],
            content_excerpt: None,
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        };
        let v: Value = serde_json::to_value(&ev).unwrap();
        // nullable → present as null
        assert!(v.get("model").unwrap().is_null());
        assert!(v.get("source_file").unwrap().is_null());
        // optional → omitted
        assert!(v.get("content_excerpt").is_none());
        assert!(v.get("content_bytes").is_none());
        // required → always present, even at its most conservative value
        // default → materialized
        assert!(v.get("tool_calls").unwrap().is_object());
        assert!(v.get("files_touched").unwrap().is_array());
        // local-only → omitted once shed, and omitted when it was never set
        assert!(v.get("tool_paths").is_none());
    }

    /// The admission rule for `tool_paths`, over the shapes tool arguments
    /// actually take. Sorted before comparing because sibling-key order is not
    /// part of the claim; dedupe and the exclusions are.
    #[test]
    fn tool_paths_are_the_separator_bearing_argument_strings() {
        let mut got = Vec::new();
        RawEvent::collect_tool_paths(
            &serde_json::json!({
                "file_path": "/Users/dev/Projects/acme/src/main.rs",
                "edits": [{ "target": "core/rust/lib.rs" }, { "target": "core/rust/lib.rs" }],
                "pattern": "TODO",
                "command": "cargo test -p acme/core",
                "docs": "https://example.test/acme/core",
                "limit": 20,
            }),
            &mut got,
        );
        got.sort();
        assert_eq!(
            got,
            vec![
                "/Users/dev/Projects/acme/src/main.rs".to_string(),
                "core/rust/lib.rs".to_string(),
            ],
            "every depth and key, deduped — and no command line, URL or bare word"
        );
    }

    #[test]
    fn clamp_truncates_abstract_bytes() {
        let mut seg = Segment {
            segment_id: "seg_x".into(),
            session_id: "s".into(),
            agent: "claude_code".into(),
            started_at: "t".into(),
            ended_at: "t".into(),
            r#abstract: "字".repeat(300), // 900 bytes > 512
            tokens: TokenUsage::default(),
            tags: vec![],
            redaction: RedactionReport::default(),
            source_event_ids: vec![],
            abstract_embedding: None,
            behavior: None,
            user_intent: None,
            local_time: None,
        };
        seg.clamp();
        assert!(seg.r#abstract.len() <= caps::ABSTRACT_MAX);
    }

    #[test]
    fn repo_anchors_is_optional_and_roundtrips() {
        // Additive: a pre-anchor batch (no key) still deserializes, and `None`
        // is omitted on output (Zod `.optional()` parity).
        let json = r#"{"batch_id":"b","device_id":"d","daemon_version":"v","events":[]}"#;
        let mut batch: IngestBatch = serde_json::from_str(json).unwrap();
        assert!(batch.repo_anchors.is_none());
        let v: Value = serde_json::to_value(&batch).unwrap();
        assert!(v.get("repo_anchors").is_none());

        batch.repo_anchors = Some(vec![RepoAnchors {
            slug: "acme/api".into(),
            host: Some("github.com".into()),
            cutoff: None,
            mined_at: "2026-06-01T10:00:00.000Z".into(),
            head_sha: "0f4c9e7d2b8a1c6f3e5d7a9b0c2d4e6f8a1b3c5d".into(),
            human_anchor_count: 1,
            ai_pr_count: 9,
            anchors: vec![AnchorPr {
                pr_number: 421,
                merge_sha: "9c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d".into(),
                merged_at: "2025-11-20T14:30:00.000Z".into(),
                files_changed: 12,
                lines_added: 340,
                lines_deleted: 85,
                span_ms: None,
                commit_count: None,
                active_minutes: None,
                ai_assisted: false,
            }],
        }]);
        let v: Value = serde_json::to_value(&batch).unwrap();
        // Unknown-when-absent anchors fields are omitted, not null.
        let anchor = &v["repo_anchors"][0]["anchors"][0];
        assert!(anchor.get("span_ms").is_none());
        assert!(anchor.get("commit_count").is_none());
        assert!(anchor.get("active_minutes").is_none());
        // …but the always-known ones materialize (Zod `.default()` parity), so
        // a consumer never has to guess whether `false`/`0` means "no" or
        // "this daemon didn't say".
        assert_eq!(anchor["ai_assisted"], Value::Bool(false));
        assert_eq!(v["repo_anchors"][0]["cutoff"], Value::Null);
        assert_eq!(v["repo_anchors"][0]["human_anchor_count"], 1);
        assert_eq!(v["repo_anchors"][0]["ai_pr_count"], 9);
        let back: IngestBatch = serde_json::from_value(v).unwrap();
        assert_eq!(batch, back);
    }

    #[test]
    fn clamp_truncates_repo_anchor_collections_and_strings() {
        let anchor = AnchorPr {
            pr_number: 1,
            merge_sha: "a".repeat(100), // > 64
            merged_at: "t".into(),
            files_changed: 0,
            lines_added: 0,
            lines_deleted: 0,
            span_ms: None,
            commit_count: None,
            active_minutes: None,
            ai_assisted: false,
        };
        let repo = RepoAnchors {
            slug: "字".repeat(100), // 300 bytes > 200
            host: None,
            cutoff: Some("t".repeat(100)), // > 40
            mined_at: "t".into(),
            head_sha: "a".repeat(100), // > 64
            human_anchor_count: 60,
            ai_pr_count: 3,
            anchors: vec![anchor; 60], // > 50
        };
        let mut batch = IngestBatch {
            batch_id: "b".into(),
            device_id: "d".into(),
            daemon_version: "v".into(),
            events: vec![],
            segments: vec![],
            tool_calls: vec![],
            session_installs: None,
            session_actors: None,
            session_titles: None,
            session_metadata: None,
            summarizer_mode: None,
            tz: Some("字".repeat(100)), // 300 bytes > 64
            tz_offset_minutes: None,
            redactor_mode: None,
            repo_anchors: Some(vec![repo; 12]), // > 10
            segment_generations: None,
        };
        batch.clamp();
        assert!(batch.tz.as_ref().unwrap().len() <= caps::TIMEZONE_MAX);
        let anchors = batch.repo_anchors.as_ref().unwrap();
        assert_eq!(anchors.len(), caps::REPO_ANCHORS_COUNT_MAX);
        assert_eq!(anchors[0].anchors.len(), caps::ANCHORS_PER_REPO_COUNT_MAX);
        assert!(anchors[0].slug.len() <= caps::ANCHOR_SLUG_MAX);
        assert!(anchors[0].head_sha.len() <= caps::ANCHOR_SHA_MAX);
        assert!(anchors[0].anchors[0].merge_sha.len() <= caps::ANCHOR_SHA_MAX);
        assert!(anchors[0].cutoff.as_ref().unwrap().len() <= caps::ANCHOR_ISO_MAX);
        // The count survives truncation on purpose: it says how many were
        // FOUND, which is the fact a thin-baseline check needs.
        assert_eq!(anchors[0].human_anchor_count, 60);
    }
}
