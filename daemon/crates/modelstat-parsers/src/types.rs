//! Shared parser types — a faithful port of `packages/parsers/src/types.ts`.
//!
//! Parsers emit [`wire::RawEvent`]s and [`ToolCallDraft`]s only; session-level
//! summarisation is the daemon pipeline's job (M3), which keeps parsers cheap
//! and deterministic.

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
    /// How this agent authenticates to its provider on this machine — one of
    /// [`modelstat_wire::enums::PRICING_MODES`], resolved once per scan by
    /// [`crate::auth_mode`] and stamped onto every event the parse emits.
    ///
    /// Lives on the context rather than being read per event because it is a
    /// property of the machine's login, not of a transcript line, and reading
    /// `auth.json` once per token_count line would be thousands of syscalls per
    /// file.
    pub pricing_mode: String,
}

impl ParserContext {
    /// A context whose pricing mode is `unknown`.
    ///
    /// Deliberately the floor, not a convenience: a caller that has not
    /// resolved the machine's auth mode does not know it, and the whole point
    /// of this field is that "not known" travels as itself instead of being
    /// rounded to a billable answer. Real scans go through
    /// [`Self::with_pricing_mode`].
    pub fn new(device_id: impl Into<String>, source_file: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            source_file: source_file.into(),
            byte_offset_start: 0,
            pricing_mode: crate::auth_mode::PRICING_MODE_UNKNOWN.to_string(),
        }
    }

    /// Same, with the machine's observed auth mode attached.
    #[must_use]
    pub fn with_pricing_mode(mut self, mode: impl Into<String>) -> Self {
        self.pricing_mode = mode.into();
        self
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
