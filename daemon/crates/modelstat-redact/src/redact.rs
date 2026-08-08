//! The wire redaction floor — port of `packages/core/src/redact.ts::redact()`.
//!
//! Pipeline (order is contractual): compiled floor (18 patterns) → additive
//! remote patterns → entropy pass → emails → absolute home paths (with repo-root
//! relativization). Counts only; the matched text never rides the wire.
//!
//! The baseline floor is unconditional and can never be removed or disabled by
//! remote config (feature §21.6); the additive `remote` patterns run AFTER it
//! and can only ADD redactions. [`redact`] reads that additive set from the
//! process ([`crate::policy`]) so no call site can opt out of it by omission.

use regex::{Captures, Regex};
use std::sync::OnceLock;

use crate::floor::{redact_aws_secret_key, regex_floor};
use crate::policy::CompiledPattern;
use crate::{entropy, paths};

/// Redaction counters — the three guaranteed fields of `RedactionReport`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RedactionCounts {
    pub secrets_found: u64,
    pub emails_redacted: u64,
    pub paths_redacted_absolute: u64,
}

/// Redacted text plus what was stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionResult {
    pub text: String,
    pub counts: RedactionCounts,
}

fn email() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // JS `/\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi` — ASCII boundaries,
    // case-insensitive.
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?-u:\b)[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}(?-u:\b)").unwrap()
    })
}

/// Redact with the baseline floor plus whatever additive augment this process
/// has installed ([`crate::policy::install_policy_patterns`]) — the call every
/// egress path makes. With nothing installed (no config fetched yet, or an empty
/// bundle) it is exactly the floor.
pub fn redact(text: &str, repo_root_abs: Option<&str>) -> RedactionResult {
    redact_with_remote(
        text,
        repo_root_abs,
        &crate::policy::installed_policy_patterns(),
    )
}

/// Redact with the baseline floor plus an explicitly supplied additive set —
/// the same pipeline [`redact`] runs, with the augment passed rather than read
/// from the process. Tests use it to pin behaviour against an exact bundle.
pub fn redact_with_remote(
    text: &str,
    repo_root_abs: Option<&str>,
    remote: &[CompiledPattern],
) -> RedactionResult {
    let mut counts = RedactionCounts::default();
    let mut out = text.to_string();

    // 1. Baseline floor — the 17 regex patterns in order, then the exact-40 AWS
    //    secret-key run scan (the 18th). Never server-weakenable.
    for pat in regex_floor() {
        let name = pat.name;
        out = pat
            .re
            .replace_all(&out, |_: &Captures| {
                counts.secrets_found += 1;
                format!("[REDACTED:{name}]")
            })
            .into_owned();
    }
    let (aws_out, aws_n) = redact_aws_secret_key(&out, "[REDACTED:aws_secret_key]");
    out = aws_out;
    counts.secrets_found += aws_n as u64;

    // 2. Additive server-delivered augment — runs after the floor, can only ADD.
    for pat in remote {
        let name = &pat.name;
        out = pat
            .re
            .replace_all(&out, |_: &Captures| {
                counts.secrets_found += 1;
                format!("[REDACTED:{name}]")
            })
            .into_owned();
    }

    // 3. Entropy pass — after named patterns so it won't double-count.
    out = entropy::apply(&out, &mut counts);

    // 4. Emails.
    out = email()
        .replace_all(&out, |_: &Captures| {
            counts.emails_redacted += 1;
            "[REDACTED:email]".to_string()
        })
        .into_owned();

    // 5. Absolute home paths (macOS/Linux/Windows) with repo-root relativization.
    out = paths::apply(&out, repo_root_abs, &mut counts);

    RedactionResult { text: out, counts }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors packages/core/src/redact.test.ts (also pinned by the generated
    // golden fixtures in tests/golden_redaction.rs).
    #[test]
    fn redacts_anthropic_keys() {
        // Split literal so the committed source matches no secret scanner; the
        // concatenation reconstitutes the synthetic key at compile time.
        let r = redact(
            concat!("use sk-ant-", "api03-abcdefghijklmnopqrstuvwxyz0123456789"),
            None,
        );
        assert!(r.text.contains("[REDACTED:anthropic_key]"));
        assert_eq!(r.counts.secrets_found, 1);
    }

    #[test]
    fn redacts_emails() {
        let r = redact("contact: alice@example.com, bob@example.co.uk", None);
        assert!(r.text.contains("[REDACTED:email]"));
        assert_eq!(r.counts.emails_redacted, 2);
    }

    #[test]
    fn relativises_inside_repo_root() {
        let r = redact(
            "see /Users/dev/projects/myrepo/src/foo.ts for details",
            Some("/Users/dev/projects/myrepo"),
        );
        assert!(r.text.contains("./src/foo.ts"));
        assert_eq!(r.counts.paths_redacted_absolute, 0);
    }

    #[test]
    fn leaves_code_identifiers_alone() {
        let r = redact(
            "function calculateTotalRevenueForQuarter(year, quarter)",
            None,
        );
        assert_eq!(r.counts.secrets_found, 0);
    }

    #[test]
    fn collapses_hashes() {
        assert!(redact("md5: 5d41402abc4b2a76b9719d911017c592", None)
            .text
            .contains("[REDACTED:hash]"));
    }

    #[test]
    fn env_secret_catches_bare_keywords() {
        let r = redact("PASSWORD=hunter2hunter2hunter2", None);
        assert!(!r.text.contains("hunter2hunter2hunter2"));
        assert!(r.text.contains("[REDACTED:env_secret]"));
    }

    #[test]
    fn env_secret_leaves_non_secrets() {
        for cmd in ["AWS_PROFILE=dev", "CHAIN_ID=1337", "MONKEY=banana"] {
            assert_eq!(
                redact(cmd, None).counts.secrets_found,
                0,
                "should not redact {cmd}"
            );
        }
    }

    #[test]
    fn keeps_relative_path_and_screaming_snake() {
        let p = redact(
            "tsx packages/daemon-core/src/queue/runner_helper_module",
            None,
        );
        assert!(p
            .text
            .contains("packages/daemon-core/src/queue/runner_helper_module"));
        let c = redact("export MAX_TOOL_ACTION_PARAM_SHAPE_CHARS_LIMIT", None);
        assert!(c.text.contains("MAX_TOOL_ACTION_PARAM_SHAPE_CHARS_LIMIT"));
    }
}
