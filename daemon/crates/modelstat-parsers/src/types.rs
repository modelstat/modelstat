//! Shared parser types.
//!
//! Parsers emit [`wire::RawEvent`]s and [`ToolCallDraft`]s only; session-level
//! summarisation is the daemon pipeline's job (M3), which keeps parsers cheap
//! and deterministic.

use std::collections::BTreeMap;

use modelstat_wire::{RawEvent, ToolAction};
use serde::{Deserialize, Serialize};

/// Upper bound on how many events a streaming parser hands to its sink per call.
/// Bounds the parser's working set: with a sink attached, at most this many
/// events exist inside the parser at any moment regardless of file size.
pub const PARSER_EVENT_CHUNK: usize = 256;

/// One extracted tool invocation, before segment attribution.
///
/// Identical to [`wire::ToolCallWire`] (and its privacy contract: hashes / byte
/// sizes / allowlisted verbs only, never payloads) **minus `segment_id`**, which
/// the daemon fills at batch-build time once segments exist — parse time is too
/// early to know it. Kept a distinct struct (not `ToolCallWire` with a null
/// `segment_id`) so it serializes byte-identically to the TS `ToolCallDraft`,
/// which omits the key entirely (`Omit<ToolCallWire, "segment_id">`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallDraft {
    pub external_call_id: String,
    pub session_id: String,
    pub source_event_id: String,
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

/// Local-only context the agent needs to summarise the script/bash FILES a
/// command runs: the RAW command + cwd.
///
/// This is the ONLY place a raw command leaves the parser, and it is NEVER
/// serialised or shipped — it rides [`ParseResult::script_contexts`] purely so
/// the pipeline can resolve + read + locally summarise referenced files into the
/// redacted `ToolAction.scripts` abstracts. Keyed to its draft by
/// `external_call_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalToolContext {
    pub external_call_id: String,
    /// Raw shell command, exactly as the agent ran it. Local-only — never shipped.
    pub command: String,
    /// Event cwd, for resolving relative script paths. Local-only — never shipped.
    pub cwd: Option<String>,
}

/// One agent-instance a harness stated it ran — the registry row behind an
/// event's `actor_id` (see [`modelstat_wire::IngestBatch::session_actors`] for
/// the wire contract each field lands under).
///
/// Every field except [`Self::id`] is present ONLY when the harness stated it.
/// Nothing here is derived, inferred, or defaulted: an absent `parent_actor_id`
/// means the harness named no parent, not that there is none — and a daemon that
/// guessed one (by splitting a path, say) would be committing every future
/// version of every harness to today's spelling of its agent tree.
///
/// Every string arrives FLOOR-REDACTED, because some of them are prompt-derived
/// (a Claude sub-agent's `description` is a sentence the caller wrote) and the
/// rule is that no captured string skips the floor. On the ids and paths the
/// pass is a no-op, which is the point: the invariant has no exceptions to
/// remember.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActor {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_actor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_depth: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ts: Option<String>,
}

impl SessionActor {
    /// Fold `other`'s statements into this entry.
    ///
    /// A stated field only ever FILLS a gap — it never overwrites a fact already
    /// recorded. One actor is described by many records (a `started` line, a
    /// `.meta.json`, then hundreds of turns) arriving across many parses, and a
    /// later record that happens to say less must not erase what an earlier one
    /// said. The two instants are the exception, and they are not really one: a
    /// span is defined by its ends, so they widen.
    pub fn absorb(&mut self, other: SessionActor) {
        fn fill<T>(slot: &mut Option<T>, v: Option<T>) {
            if slot.is_none() {
                *slot = v;
            }
        }
        fill(&mut self.label, other.label);
        fill(&mut self.description, other.description);
        fill(&mut self.path, other.path);
        fill(&mut self.thread_id, other.thread_id);
        fill(&mut self.parent_actor_id, other.parent_actor_id);
        fill(&mut self.spawn_tool_use_id, other.spawn_tool_use_id);
        fill(&mut self.spawn_depth, other.spawn_depth);
        // ISO-8601 UTC instants, so lexical order IS chronological order — the
        // one property that lets a span widen without parsing a date.
        if let Some(ts) = other.first_ts {
            if self.first_ts.as_ref().is_none_or(|cur| ts < *cur) {
                self.first_ts = Some(ts);
            }
        }
        if let Some(ts) = other.last_ts {
            if self.last_ts.as_ref().is_none_or(|cur| ts > *cur) {
                self.last_ts = Some(ts);
            }
        }
    }
}

/// The actor registry a parse assembled: `session_id -> actor_id -> facts`.
///
/// Keyed by id rather than a list so repeated sightings of one actor fold
/// together as they are read; the wire shape (a list per session) is produced at
/// the batch door.
pub type SessionActors = BTreeMap<String, BTreeMap<String, SessionActor>>;

/// Record one actor sighting into a registry, folding it into whatever that
/// actor's id already said ([`SessionActor::absorb`]).
pub fn record_actor(actors: &mut SessionActors, session_id: &str, actor: SessionActor) {
    let entry = actors
        .entry(session_id.to_string())
        .or_default()
        .entry(actor.id.clone())
        .or_insert_with(|| SessionActor {
            id: actor.id.clone(),
            ..SessionActor::default()
        });
    entry.absorb(actor);
}

/// Fold one parse's registry into a run-long one (the scan accumulates across
/// files and flushes exactly as it does for segments and turns).
pub fn merge_session_actors(into: &mut SessionActors, from: SessionActors) {
    for (session_id, actors) in from {
        for (_, actor) in actors {
            record_actor(into, &session_id, actor);
        }
    }
}

/// Per-file parse statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseStats {
    #[serde(rename = "rawLines")]
    pub raw_lines: u64,
    #[serde(rename = "emittedEvents")]
    pub emitted_events: u64,
    pub skipped: u64,
}

/// What a parser produces for a single source file (or SQLite row-set).
#[derive(Debug, Clone, Default)]
pub struct ParseResult {
    /// All parsed events — EMPTY when the caller attached a streaming sink
    /// (see [`Sink`]), so a multi-hundred-MB transcript never materialises as
    /// one giant array.
    pub events: Vec<RawEvent>,
    /// Per-call tool invocations extracted from the source. Empty for sources
    /// without tool-call data (Cursor).
    pub tool_calls: Vec<ToolCallDraft>,
    /// Local-only per-call contexts (raw command + cwd) for the script-summary
    /// enrichment pass. Empty for sources with no shell calls. NEVER shipped.
    pub script_contexts: Vec<LocalToolContext>,
    pub stats: ParseStats,
    /// Every record this parse DROPPED, counted under the kind string the record
    /// states about itself — see [`crate::skips`]. `stats.skipped` is the same
    /// event as one number; this says which dialects it was.
    pub skipped_kinds: BTreeMap<String, u64>,
    /// Every agent-instance this parse saw the source state, per session — the
    /// registry the events' `actor_id`s join against. Empty for a source whose
    /// harness has no concept of more than one agent.
    pub session_actors: SessionActors,
    /// Source file path (for dedupe + replay).
    pub source_file: String,
}

/// Everything a parser needs to parse one file.
#[derive(Debug, Clone)]
pub struct ParserContext {
    /// Stable device id.
    pub device_id: String,
    /// Absolute path to the file being parsed (or a synthetic key for DBs).
    pub source_file: String,
    /// For incremental parsers: skip bytes before this offset.
    pub byte_offset_start: u64,
    /// For non-positional sources (Cursor's key/value chat store): skip records
    /// older than this instant, which the scan already shipped. `None` reads
    /// everything. Positional parsers ignore it — their floor is a byte offset
    /// applied to the SEND, since they carry cross-line state that a mid-file
    /// start would lose; a key/value row carries none, so skipping is safe.
    pub since_ms: Option<i64>,
    /// Overrides the agent name the parse stamps on every event. Set when a
    /// HOST runs another agent's binary in its own format — Claude Desktop's
    /// local agent mode writes Claude Code transcripts, but the human used
    /// Claude Desktop. `None` keeps the parser's own name.
    pub agent_label: Option<String>,
}

impl ParserContext {
    pub fn new(device_id: impl Into<String>, source_file: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            source_file: source_file.into(),
            byte_offset_start: 0,
            since_ms: None,
            agent_label: None,
        }
    }

    /// See [`ParserContext::since_ms`].
    #[must_use]
    pub fn with_since_ms(mut self, since_ms: Option<i64>) -> Self {
        self.since_ms = since_ms;
        self
    }

    /// See [`ParserContext::agent_label`].
    #[must_use]
    pub fn with_agent_label(mut self, agent_label: Option<String>) -> Self {
        self.agent_label = agent_label;
        self
    }

    /// The agent name to stamp: the host's label when one was set, else the
    /// parser's own `default`.
    #[must_use]
    pub fn agent(&self, default: &str) -> String {
        self.agent_label
            .clone()
            .unwrap_or_else(|| default.to_string())
    }
}

/// Where parsed events go. `Collect` accumulates them on [`ParseResult::events`];
/// `Stream` delivers them in chunks of at most [`PARSER_EVENT_CHUNK`] to a sink
/// and leaves `events` empty — the memory contract for full-corpus reprocesses.
///
/// Both modes produce byte-identical events (the streaming-equivalence property,
/// M2 AC); the only difference is where they land.
pub enum Sink<'a> {
    Collect(Vec<RawEvent>),
    Stream {
        chunk: Vec<RawEvent>,
        emit: &'a mut dyn FnMut(Vec<RawEvent>),
    },
}

impl<'a> Sink<'a> {
    pub fn collect() -> Self {
        Sink::Collect(Vec::new())
    }

    pub fn stream(emit: &'a mut dyn FnMut(Vec<RawEvent>)) -> Self {
        Sink::Stream {
            chunk: Vec::new(),
            emit,
        }
    }

    /// Push one event. In stream mode, flushes a full [`PARSER_EVENT_CHUNK`].
    pub fn push(&mut self, event: RawEvent) {
        match self {
            Sink::Collect(v) => v.push(event),
            Sink::Stream { chunk, emit } => {
                chunk.push(event);
                if chunk.len() >= PARSER_EVENT_CHUNK {
                    emit(std::mem::take(chunk));
                }
            }
        }
    }

    /// Flush any partial chunk (stream mode only; a no-op for collect mode).
    /// Call once at end-of-parse before reading the collected events.
    pub fn flush(&mut self) {
        if let Sink::Stream { chunk, emit } = self {
            if !chunk.is_empty() {
                emit(std::mem::take(chunk));
            }
        }
    }

    /// Take the collected events (empty in stream mode).
    pub fn take_collected(&mut self) -> Vec<RawEvent> {
        match self {
            Sink::Collect(v) => std::mem::take(v),
            Sink::Stream { .. } => Vec::new(),
        }
    }
}
