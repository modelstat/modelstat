//! Byte-offset-tracking line reader shared by the JSONL parsers.
//!
//! Reproduces the TS parsers' offset accounting exactly: each line's advance is
//! `Buffer.byteLength(line_without_newline, "utf8") + 1` (the `+1` is the stripped
//! `\n`), and `source_byte_offset` is captured *before* the advance. A trailing
//! `\r` is stripped (Node readline's `crlfDelay: Infinity`) — so on an LF file
//! (every fixture) offsets are exact, and on a CRLF file they drift identically
//! to the TS, preserving `source_event_id` parity either way.

use std::io::BufRead;

/// Yields `(line, offset)` pairs, where `line` is the decoded content without a
/// trailing `\n`/`\r` and `offset` is the byte position of the line's start.
pub struct OffsetLines<R: BufRead> {
    reader: R,
    pos: u64,
}

impl<R: BufRead> OffsetLines<R> {
    pub fn new(reader: R, start_offset: u64) -> Self {
        Self {
            reader,
            pos: start_offset,
        }
    }

    /// Read the next line. Returns `Ok(None)` at EOF.
    pub fn next_line(&mut self) -> std::io::Result<Option<(String, u64)>> {
        let mut raw: Vec<u8> = Vec::new();
        let n = self.reader.read_until(b'\n', &mut raw)?;
        if n == 0 {
            return Ok(None);
        }
        // Strip the trailing `\n`, then a trailing `\r` (CRLF).
        if raw.last() == Some(&b'\n') {
            raw.pop();
        }
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
        let byte_len = raw.len() as u64 + 1; // TS: byteLength(line) + 1
        let offset = self.pos;
        self.pos += byte_len;
        let line = String::from_utf8_lossy(&raw).into_owned();
        Ok(Some((line, offset)))
    }
}
