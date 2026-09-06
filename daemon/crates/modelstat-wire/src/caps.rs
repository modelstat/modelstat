//! Frozen wire caps (feature §17.3, §18). Every bounded string on the wire has a
//! declared maximum that the server enforces in **UTF-8 bytes**; the daemon
//! clamps to the same numbers in bytes (see [`crate::clamp`]) so no payload can
//! be permanently 400'd and wedge a cursor (the CJK-abstract bug, §17.2).
//!
//! Named constants so drift is a diff, not an incident (plan §6): a test names
//! each one. The values mirror the Zod `.max(N)` in `packages/core/src/schemas.ts`.

// --- RawEvent -------------------------------------------------------------
pub const MODEL_MAX: usize = 120;
pub const SESSION_ID_MAX: usize = 120;
pub const FILES_TOUCHED_ITEM_MAX: usize = 512;
pub const FILES_TOUCHED_COUNT_MAX: usize = 256;
// SPEC 0005: a real message body, not summarizer feed — VERBATIM redacted
// text, never processed short. This bound exists only as an extreme
// malicious-size guard (was 320 pre-capture); no real message approaches it.
pub const CONTENT_EXCERPT_MAX: usize = 262_144;
// The model's own reasoning for a turn is a message body like any other, and it
// is bounded for the same single reason: an extreme malicious-size guard. Same
// number as the prose deliberately — two different ceilings would say the two
// texts are different kinds of thing, and they are not.
pub const REASONING_EXCERPT_MAX: usize = 262_144;
pub const SOURCE_FILE_MAX: usize = 1024;
/// An RFC3339 instant stated ALONGSIDE `ts` (`started_at`, `first_token_at`).
/// The same number as [`ANCHOR_ISO_MAX`] on purpose — one shape, one ceiling.
///
/// A SIZE guard, not a format check, which is the same treatment `ts` gets: an
/// instant these fields cannot parse is one the server rejects for the whole
/// batch, and that is the wire contract doing its job. Validating the format
/// here — while the required instant beside them has no validator — would only
/// move where the same batch fails.
pub const EVENT_INSTANT_MAX: usize = 40;

// --- Segment --------------------------------------------------------------
pub const SEGMENT_ID_MAX: usize = 64;
pub const ABSTRACT_MAX: usize = 512;
pub const TAGS_COUNT_MAX: usize = 40;
pub const SOURCE_EVENT_IDS_COUNT_MAX: usize = 2000;
pub const ABSTRACT_EMBEDDING_LEN: usize = 384;
pub const USER_INTENT_MAX: usize = 512;

// --- TaxonomyHintRooted ---------------------------------------------------
pub const TAG_ROOT_KEY_MAX: usize = 60;
pub const TAG_NAME_MAX: usize = 120;
pub const TAG_REASON_MAX: usize = 200;

// --- ToolAction -----------------------------------------------------------
pub const TA_SURFACE_MAX: usize = 40;
pub const TA_EXECUTABLE_MAX: usize = 80;
pub const TA_ACTION_MAX: usize = 40;
pub const TA_OBJECT_MAX: usize = 60;
pub const TA_QUALIFIER_ITEM_MAX: usize = 40;
pub const TA_QUALIFIERS_COUNT_MAX: usize = 8;
pub const TA_PARAM_SHAPE_MAX: usize = 16_384;
pub const TA_KEYWORD_ITEM_MAX: usize = 40;
pub const TA_KEYWORDS_COUNT_MAX: usize = 12;
pub const TA_ABSTRACT_MAX: usize = 200;
pub const TA_INPUT_FORMAT_MAX: usize = 8;
pub const TA_SCRIPT_TOKEN_MAX: usize = 200;
pub const TA_SCRIPT_SUMMARY_MAX: usize = 200;
pub const TA_SCRIPTS_COUNT_MAX: usize = 8;
pub const TA_EXTRACTOR_MAX: usize = 40;

// --- ToolCallWire ---------------------------------------------------------
pub const EXTERNAL_CALL_ID_MAX: usize = 120;
pub const SERVER_MAX: usize = 120;
pub const NAME_MAX: usize = 120;
pub const ARGS_HASH_MAX: usize = 64;
pub const SIGNATURE_HASH_MAX: usize = 64;
/// The `mcp:<server>` name is clamped to 116 before the `mcp:` prefix so the
/// whole `server` field fits its 120 cap (tool-hash `splitObservedToolName`).
pub const MCP_SERVER_NAME_MAX: usize = 116;

// --- IngestBatch ----------------------------------------------------------
pub const DAEMON_VERSION_MAX: usize = 40;
/// Highest `processing_version` the server can store — the width of its
/// `events.producer_version` column.
///
/// The one cap here that is NOT a length, and the one the daemon must never
/// reach by clamping. A generation folded onto this ceiling ties with every
/// other generation folded onto it, on exactly the `ReplacingMergeTree` version
/// that stating a generation exists to break — so the server REFUSES a batch
/// above it rather than truncating, and the daemon proves at compile time that
/// its own number fits (`modelstat_ingest::processing`).
pub const PROCESSING_VERSION_MAX: u32 = u16::MAX as u32;
pub const EVENTS_COUNT_MAX: usize = 10_000;
pub const SEGMENTS_COUNT_MAX: usize = 2_000;
pub const TOOL_CALLS_COUNT_MAX: usize = 20_000;
pub const SESSION_TITLE_MAX: usize = 120;

// --- RepoAnchors / AnchorPr -------------------------------------------------
pub const ANCHOR_SLUG_MAX: usize = 200;
pub const ANCHOR_HOST_MAX: usize = 80;
pub const ANCHOR_SHA_MAX: usize = 64;
pub const ANCHOR_ISO_MAX: usize = 40;
pub const ANCHORS_PER_REPO_COUNT_MAX: usize = 50;
pub const REPO_ANCHORS_COUNT_MAX: usize = 10;

// --- HeartbeatPayload -----------------------------------------------------
pub const HEARTBEAT_MESSAGE_MAX: usize = 240;
/// An IANA time-zone name (`Europe/Berlin`, `America/Argentina/Buenos_Aires`).
/// The longest name in the database is well under this; the cap is the usual
/// malicious-size guard, not a claim about the zone database's contents.
pub const TIMEZONE_MAX: usize = 64;

// --- DetectedInstallation / DetectedIdentity ------------------------------
pub const INSTALL_VERSION_MAX: usize = 40;
pub const DETECTED_VIA_COUNT_MAX: usize = 6;
pub const PROVIDER_ACCOUNT_ID_MAX: usize = 200;
pub const DETECTION_SOURCE_MAX: usize = 80;
