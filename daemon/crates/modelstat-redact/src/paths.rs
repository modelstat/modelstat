//! Absolute-home-path redaction with repo-root relativization.
//!
//! macOS (`/Users/…`) and Linux (`/home/…`) are a direct port of `redact.ts`.
//! feature §17.4 additionally mandates Windows home-path shapes — `C:\Users\…`,
//! `%USERPROFILE%`, and UNC `\\host\share`. Those are NEW (the TS had none), so
//! they have no TS golden; they are covered by Rust unit tests here and called
//! out as a spec-required addition in daemon/PARITY.md. They are additive
//! (strictly more privacy) and land under PROCESSING_VERSION 16.
//!
//! Rule (all platforms): a matched path under the session's `repo_root` is
//! rewritten to `./…` and NOT counted; any other absolute path becomes
//! `[REDACTED:abs-path]` and increments `paths_redacted_absolute`.

use regex::{Captures, Regex};
use std::sync::OnceLock;

use crate::RedactionCounts;

fn macos() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"/Users/[^\s"'`)]+"#).unwrap())
}
fn linux() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"/home/[^\s"'`)]+"#).unwrap())
}
fn win_userprofile() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Drive-letter home path: `C:\Users\…` (backslash-separated).
    RE.get_or_init(|| Regex::new(r#"[A-Za-z]:\\Users\\[^\s"'`)]+"#).unwrap())
}
fn win_env() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `%USERPROFILE%` optionally followed by a path tail.
    RE.get_or_init(|| Regex::new(r#"%USERPROFILE%[^\s"'`)]*"#).unwrap())
}
fn win_unc() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // UNC share: `\\host\share…`.
    RE.get_or_init(|| Regex::new(r#"\\\\[^\s"'`)]+\\[^\s"'`)]+"#).unwrap())
}
fn leading_seps_unix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^/+").unwrap())
}
fn leading_seps_any() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[/\\]+").unwrap())
}

/// Apply one path pattern with the shared replacer semantics.
fn apply_one(
    re: &Regex,
    input: &str,
    repo_root: Option<&str>,
    leading: &Regex,
    counts: &mut RedactionCounts,
) -> String {
    re.replace_all(input, |caps: &Captures| {
        let m = &caps[0];
        if let Some(root) = repo_root {
            if let Some(rest) = m.strip_prefix(root) {
                // Under the repo root → relativize, uncounted.
                return leading.replace(rest, "./").into_owned();
            }
        }
        counts.paths_redacted_absolute += 1;
        "[REDACTED:abs-path]".to_string()
    })
    .into_owned()
}

/// Run every path pass in order (macOS, Linux, then the Windows additions),
/// mirroring `redact.ts`'s macOS-then-Linux ordering.
pub(crate) fn apply(input: &str, repo_root: Option<&str>, counts: &mut RedactionCounts) -> String {
    let mut out = apply_one(macos(), input, repo_root, leading_seps_unix(), counts);
    out = apply_one(linux(), &out, repo_root, leading_seps_unix(), counts);
    // Windows additions (feature §17.4). Backslash-aware relativization.
    out = apply_one(
        win_userprofile(),
        &out,
        repo_root,
        leading_seps_any(),
        counts,
    );
    out = apply_one(win_env(), &out, repo_root, leading_seps_any(), counts);
    out = apply_one(win_unc(), &out, repo_root, leading_seps_any(), counts);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &str, root: Option<&str>) -> (String, u64) {
        let mut c = RedactionCounts::default();
        let out = apply(input, root, &mut c);
        (out, c.paths_redacted_absolute)
    }

    #[test]
    fn macos_outside_root_redacted() {
        let (out, n) = run(
            "open /Users/dev/secrets/db.conf",
            Some("/Users/dev/projects/myrepo"),
        );
        assert!(out.contains("[REDACTED:abs-path]"));
        assert_eq!(n, 1);
    }

    #[test]
    fn macos_inside_root_relativized_uncounted() {
        let (out, n) = run(
            "see /Users/dev/projects/myrepo/src/foo.ts here",
            Some("/Users/dev/projects/myrepo"),
        );
        assert!(out.contains("./src/foo.ts"));
        assert_eq!(n, 0);
    }

    #[test]
    fn windows_userprofile_redacted() {
        let (out, n) = run(r"load C:\Users\mark\secrets.txt now", None);
        assert!(out.contains("[REDACTED:abs-path]"), "got: {out}");
        assert_eq!(n, 1);
    }

    #[test]
    fn windows_env_and_unc_redacted() {
        let (out, n) = run(r"%USERPROFILE%\.ssh and \\fileserver\share\x", None);
        assert_eq!(n, 2, "got: {out}");
    }
}
