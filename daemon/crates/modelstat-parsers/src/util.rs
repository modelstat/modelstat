//! Small shared string helpers ported from the TS parsers' excerpt/name paths,
//! plus a timeout-bounded subprocess runner used by the git + discovery probes.

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;

/// Run `program args` (optionally in `cwd`), returning stdout on a zero exit
/// within `timeout`, else None. Best-effort: any spawn/exit/timeout failure is
/// None (probes must never block or fail a scan). A reader thread drains stdout
/// so the child never blocks on a full pipe; on timeout the child is killed.
pub fn run_command(
    program: &str,
    args: &[&str],
    cwd: Option<&str>,
    timeout: Duration,
) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                break None;
            }
        }
    };
    let out = reader.join().ok()?;
    match status {
        Some(s) if s.success() => Some(out),
        _ => None,
    }
}

/// Take at most `max` UTF-16 code units of `s`, matching JS `String.slice(0,max)`
/// (which slices UTF-16 units, not code points). BMP-only inputs slice identically
/// to a char count.
pub fn slice_utf16(s: &str, max: usize) -> String {
    if s.encode_utf16().count() <= max {
        return s.to_string();
    }
    let units: Vec<u16> = s.encode_utf16().take(max).collect();
    String::from_utf16_lossy(&units)
}

/// Strip fenced ``` code blocks ``` and inline `code` spans, replacing each with
/// a single space — JS `text.replace(/```[\s\S]*?```/g," ").replace(/`[^`]*`/g," ")`.
pub fn strip_code(text: &str) -> String {
    static FENCE: OnceLock<Regex> = OnceLock::new();
    static INLINE: OnceLock<Regex> = OnceLock::new();
    let fence = FENCE.get_or_init(|| Regex::new(r"```[\s\S]*?```").unwrap());
    let inline = INLINE.get_or_init(|| Regex::new(r"`[^`]*`").unwrap());
    let a = fence.replace_all(text, " ");
    inline.replace_all(&a, " ").into_owned()
}

/// Collapse every whitespace run to a single space and trim — JS
/// `text.replace(/\s+/g," ").trim()`.
pub fn collapse_ws(text: &str) -> String {
    static WS: OnceLock<Regex> = OnceLock::new();
    let ws = WS.get_or_init(|| Regex::new(r"\s+").unwrap());
    ws.replace_all(text, " ").trim().to_string()
}
