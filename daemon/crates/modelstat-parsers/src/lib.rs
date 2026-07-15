//! Session-log parsers (claude-code / codex / pi / cursor), discovery,
//! tool-action/shell.v3, tool-hash, references, and git helpers — the M2 port of
//! `packages/parsers` + the `@modelstat/core` reference miner
//! (see core/specs/daemon/plan.md §5, feature §7).
//!
//! Every parser is a byte-for-byte port: given the same canonical input paths it
//! reproduces the TS `RawEvent`/`ToolCallDraft` JSON pinned in the golden
//! fixtures (`modelstat-wire/tests/golden/parsers/`). The heavy parity subtleties
//! (UTF-8 byte offsets, resume-copy dedupe, disjoint codex token buckets,
//! `JSON.stringify`-order arg hashing) are documented at each call site and in
//! `daemon/PARITY.md`.

pub(crate) mod line_reader;
pub(crate) mod util;

pub mod references;
pub mod tool_action;
pub mod tool_hash;
pub mod types;

pub mod git;

pub mod claude_code;
pub mod codex;
pub mod cursor;
pub mod pi;

// Flat re-exports for the common surface.
pub use tool_action::{
    detect_script_refs, extract_local_tool_context, extract_tool_action, resolve_script_path,
    script_candidates, ToolActionInput, OTHER_BUCKET,
};
pub use tool_hash::{
    hash_args, json_bytes, normalize_tool_name, split_observed_tool_name, tool_identity,
    ArgsHashes, SIGNATURE_NONE,
};
pub use types::{
    LocalToolContext, ParseResult, ParseStats, ParserContext, Sink, ToolCallDraft,
    PARSER_EVENT_CHUNK,
};

pub use claude_code::{
    decode_encoded_dir, derive_session_id_from_filename, parse_claude_code_jsonl, quick_checksum,
    QuickChecksum,
};
pub use codex::parse_codex_rollout;
pub use cursor::parse_cursor_tracking_db;
pub use pi::parse_pi_session;
