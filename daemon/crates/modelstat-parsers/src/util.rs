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

// --- SPEC 0005: paste-aware verbatim cleaning ------------------------------

/// A fenced block bigger than either bound is pasted payload, not typed text.
pub const PASTED_FENCE_MAX_LINES: usize = 12;
pub const PASTED_FENCE_MAX_BYTES: usize = 1_024;
/// An unfenced paragraph (blank-line-delimited) bigger than either bound is a
/// paste (log dump, stack trace, file body) — typed prose never looks like it.
pub const PASTED_PARA_MAX_LINES: usize = 15;
pub const PASTED_PARA_MAX_BYTES: usize = 1_536;

/// Drop platform-injected blocks the developer never typed: system reminders,
/// slash-command wrappers (the typed command NAME/ARGS are kept, the machinery
/// is not), and local command output. Deterministic and structural — the
/// injection markers are exact tags, not guessed content.
pub fn strip_injected(text: &str) -> String {
    static DROP: OnceLock<Regex> = OnceLock::new();
    static UNWRAP_TAGS: OnceLock<Regex> = OnceLock::new();
    // Whole blocks the developer never typed (no backreferences — the regex
    // crate has none; each pair is spelled out).
    let drop = DROP.get_or_init(|| {
        Regex::new(
            r"(?s)<system-reminder>.*?</system-reminder>|<command-message>.*?</command-message>|<local-command-stdout>.*?</local-command-stdout>|<local-command-stderr>.*?</local-command-stderr>",
        )
        .unwrap()
    });
    // The typed command NAME/ARGS keep their content, lose the wrappers.
    let unwrap_tags =
        UNWRAP_TAGS.get_or_init(|| Regex::new(r"</?command-name>|</?command-args>").unwrap());
    let a = drop.replace_all(text, "");
    unwrap_tags.replace_all(&a, "").into_owned()
}

/// Replace pasted payload with explicit `[pasted: N lines, N KB]` markers,
/// keeping what the developer actually TYPED verbatim: small code fences and
/// inline code are typed and survive; big fences and paste-shaped unfenced
/// paragraphs are elided. The marker says something was here — analyses never
/// mistake elision for absence.
pub fn elide_pastes(text: &str) -> String {
    static FENCE: OnceLock<Regex> = OnceLock::new();
    let fence = FENCE.get_or_init(|| Regex::new(r"```[\s\S]*?```").unwrap());
    let fenced = fence.replace_all(text, |c: &regex::Captures| {
        let block = c.get(0).map_or("", |m| m.as_str());
        if block.lines().count() > PASTED_FENCE_MAX_LINES || block.len() > PASTED_FENCE_MAX_BYTES {
            paste_marker(block)
        } else {
            block.to_string()
        }
    });
    // Paragraph pass over what's left: blank-line-delimited blocks that are
    // paste-shaped (huge or very tall) get the same marker. Fences already
    // handled above are small by construction and never trip the bounds.
    let mut out: Vec<String> = Vec::new();
    for para in fenced.split("\n\n") {
        let lines = para.lines().count();
        if para.len() > PASTED_PARA_MAX_BYTES || lines > PASTED_PARA_MAX_LINES {
            out.push(paste_marker(para));
        } else {
            out.push(para.to_string());
        }
    }
    out.join("\n\n")
}

fn paste_marker(block: &str) -> String {
    let lines = block.lines().count().max(1);
    let kb = (block.len() as f64 / 1024.0).max(0.1);
    format!("[pasted: {lines} lines, {kb:.1} KB]")
}

/// Tidy verbatim text: trim ends, collapse 3+ consecutive blank lines to one
/// blank line. (The old excerpt path flattened ALL whitespace — that destroyed
/// the message's structure; verbatim capture keeps it.)
pub fn tidy_verbatim(text: &str) -> String {
    static BLANKS: OnceLock<Regex> = OnceLock::new();
    let blanks = BLANKS.get_or_init(|| Regex::new(r"\n{3,}").unwrap());
    blanks.replace_all(text.trim(), "\n\n").into_owned()
}

/// Slice to at most `max` UTF-16 units; when it had to cut, the tail says so
/// (`… [+N chars]`) instead of ending mid-sentence silently. The marker fits
/// inside the cap, never on top of it.
pub fn cap_with_marker(s: &str, max: usize) -> String {
    let total = s.encode_utf16().count();
    if total <= max {
        return s.to_string();
    }
    let marker = format!(" [+{} chars]", total.saturating_sub(max));
    let keep = max.saturating_sub(marker.encode_utf16().count());
    let mut out = slice_utf16(s, keep);
    out.push_str(&marker);
    out
}

#[cfg(test)]
mod spec0005_tests {
    use super::*;

    #[test]
    fn strip_injected_drops_noise_keeps_typed_command() {
        let text = "<command-name>/deploy</command-name><command-args>prod</command-args>\
<command-message>deploy is running…</command-message>\
<local-command-stdout>lots of output</local-command-stdout>\n\
also fix the login bug<system-reminder>be terse</system-reminder>";
        let out = strip_injected(text);
        assert!(out.contains("/deploy"));
        assert!(out.contains("prod"));
        assert!(out.contains("also fix the login bug"));
        assert!(!out.contains("deploy is running"));
        assert!(!out.contains("lots of output"));
        assert!(!out.contains("be terse"));
        assert!(!out.contains('<'), "no wrapper tags survive: {out}");
    }

    #[test]
    fn elide_pastes_keeps_small_fences_elides_big_ones() {
        let small = "check this:\n```rust\nfn a() {}\n```\nplease";
        assert_eq!(
            elide_pastes(small),
            small,
            "a typed snippet survives verbatim"
        );

        let big_body = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let big = format!("here's the log:\n```\n{big_body}\n```");
        let out = elide_pastes(&big);
        assert!(out.starts_with("here's the log:"));
        assert!(out.contains("[pasted: "), "big fence → marker: {out}");
        assert!(!out.contains("line 33"));
    }

    #[test]
    fn elide_pastes_catches_unfenced_dumps() {
        let dump = (0..30)
            .map(|i| format!("at frame::{i} something exploded"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("why does this happen?\n\n{dump}");
        let out = elide_pastes(&text);
        assert!(out.starts_with("why does this happen?"));
        assert!(
            out.contains("[pasted: 30 lines"),
            "paste-shaped paragraph → marker: {out}"
        );
    }

    #[test]
    fn cap_with_marker_says_what_it_cut() {
        let s = "a".repeat(500);
        let out = cap_with_marker(&s, 100);
        assert!(out.encode_utf16().count() <= 100);
        assert!(out.ends_with("chars]"), "{out}");
        assert_eq!(cap_with_marker("short", 100), "short");
    }

    #[test]
    fn tidy_verbatim_keeps_structure() {
        let out = tidy_verbatim("  a\n\n\n\n\nb  ");
        assert_eq!(out, "a\n\nb", "newlines survive, runs collapse");
    }
}
