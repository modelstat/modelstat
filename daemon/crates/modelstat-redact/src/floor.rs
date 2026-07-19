//! The compiled-in secret floor — a byte-for-byte port of
//! `packages/core/src/redact-floor.ts` (the catalogue) applied the way
//! `packages/core/src/redact.ts` applies it (the wire floor: the WHOLE match
//! becomes `[REDACTED:<name>]`, regardless of the catalogue's replacement
//! template — that template is for the SDK-side redactor, ported later).
//!
//! Order is specific → generic and is contractual: a known key must be labelled
//! precisely before the generic env-secret / 40-char-blob catchers claim it. The
//! 18th pattern (`aws_secret_key`) uses JS lookaround (`(?<!…)…(?!…)`) that the
//! `regex` crate can't express; it is implemented as an exact-length run scan
//! (see [`redact_aws_secret_key`]), which is provably equivalent — a lookaround-
//! bounded run of exactly 40 class chars.
//!
//! Regex-engine parity notes vs JavaScript:
//!   * JS `\b`, `\d`, `\w` are ASCII. The `regex` crate defaults to Unicode, so
//!     boundaries use `(?-u:\b)` and `\d`/`\w` are spelled out as ASCII classes.
//!   * `\s` (whitespace) is left Unicode — a superset of JS `\s` that agrees on
//!     every realistic input; the golden fixtures pin the behavior.

use regex::Regex;
use std::sync::OnceLock;

/// A single floor entry: its stable label and its compiled detector.
pub struct FloorPattern {
    pub name: &'static str,
    pub re: Regex,
}

/// The catalogue's replacement templates, kept as DATA for the SDK-side redactor
/// (ported in M3) and for documentation — the wire floor here does not use them.
/// Index-aligned with the ordered pattern list below (minus `aws_secret_key`,
/// which the wire floor redacts whole like the rest).
pub const FLOOR_REPLACEMENT_TEMPLATES: &[(&str, &str)] = &[
    ("anthropic_key", "<REDACTED:anthropic_key>"),
    ("openai_key", "<REDACTED:openai_key>"),
    ("google_api_key", "<REDACTED:google_api_key>"),
    ("aws_access_key", "<REDACTED:aws_access_key>"),
    ("github_pat", "<REDACTED:github_pat>"),
    ("github_oauth", "<REDACTED:github_oauth>"),
    ("github_app", "<REDACTED:github_app>"),
    ("slack_token", "<REDACTED:slack_token>"),
    ("stripe_live_key", "<REDACTED:stripe_live_key>"),
    ("stripe_test_key", "<REDACTED:stripe_test_key>"),
    ("discord_token", "<REDACTED:discord_token>"),
    ("jwt", "<REDACTED:jwt>"),
    ("private_key_header", "<REDACTED:private_key>"),
    (
        "modelstat_device_secret",
        "<REDACTED:modelstat_device_secret>",
    ),
    ("env_secret", "$1=<REDACTED:env_secret>"),
    ("bearer_header", "Bearer <REDACTED:bearer>"),
    (
        "db_url_with_password",
        "$1://<user>:<REDACTED:db_password>@",
    ),
    ("aws_secret_key", "<REDACTED:aws_secret_key>"),
];

/// The 17 regex floor patterns, in order (the 18th, `aws_secret_key`, is the run
/// scan below). Compiled once.
pub fn regex_floor() -> &'static [FloorPattern] {
    static FLOOR: OnceLock<Vec<FloorPattern>> = OnceLock::new();
    FLOOR.get_or_init(|| {
        let specs: &[(&str, &str)] = &[
            ("anthropic_key", r"sk-ant-[A-Za-z0-9_-]{20,}"),
            ("openai_key", r"sk-(?:proj-)?[A-Za-z0-9_-]{20,}"),
            ("google_api_key", r"AIza[0-9A-Za-z_-]{35}"),
            ("aws_access_key", r"(?-u:\b)(?:AKIA|ASIA)[0-9A-Z]{16}(?-u:\b)"),
            ("github_pat", r"ghp_[A-Za-z0-9]{36,}"),
            ("github_oauth", r"gho_[A-Za-z0-9]{36,}"),
            ("github_app", r"gh[sur]_[A-Za-z0-9]{36,}"),
            ("slack_token", r"xox[aboprs]-[A-Za-z0-9-]{10,}"),
            ("stripe_live_key", r"(?:sk|pk|rk)_live_[A-Za-z0-9]{24,}"),
            ("stripe_test_key", r"(?:sk|pk|rk)_test_[A-Za-z0-9]{24,}"),
            // JS `[MN][A-Za-z\d]{23}\.[\w-]{6}\.[\w-]{27}` with ASCII \d,\w.
            (
                "discord_token",
                r"[MN][A-Za-z0-9]{23}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27}",
            ),
            (
                "jwt",
                r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            ),
            (
                "private_key_header",
                r"-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----[\s\S]*?-----END (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----",
            ),
            ("modelstat_device_secret", r"ds_live_[A-Za-z0-9_-]{32,}"),
            (
                "env_secret",
                r#"(?-u:\b)([A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD|PASSWD|PASSPHRASE|CREDENTIAL|API)[A-Z0-9_]*)\s*[:=]\s*['"]?([^\s'"]{12,})['"]?"#,
            ),
            ("bearer_header", r"Bearer\s+[A-Za-z0-9._~+/-]{20,}=*"),
            // Case-insensitive (JS `gi`); ASCII boundary.
            (
                "db_url_with_password",
                r"(?i)(?-u:\b)(postgres|mysql|mongodb|redis|amqp)(?:\+[a-z]+)?://[^:\s]+:([^@\s]+)@",
            ),
        ];
        specs
            .iter()
            .map(|(name, src)| FloorPattern {
                name,
                re: Regex::new(src)
                    .unwrap_or_else(|e| panic!("floor pattern {name} failed to compile: {e}")),
            })
            .collect()
    })
}

/// True iff `c` is in the AWS-secret class `[A-Za-z0-9/+=]`.
fn is_aws_class(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '/' || c == '+' || c == '='
}

/// The 18th floor pattern (`aws_secret_key`): JS
/// `/(?<![A-Za-z0-9/+=])[A-Za-z0-9/+=]{40}(?![A-Za-z0-9/+=])/g`.
///
/// The lookarounds make this match exactly a maximal run of 40 class chars (a
/// run of length <40 never matches; a run of length >40 has no offset where both
/// lookarounds hold; a run of exactly 40 matches once). So: find maximal runs of
/// `[A-Za-z0-9/+=]` and redact those whose length is exactly 40. Returns the new
/// string and how many runs were redacted (added to `secrets_found`).
pub fn redact_aws_secret_key(input: &str, marker: &str) -> (String, usize) {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut count = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if is_aws_class(chars[i]) {
            let start = i;
            while i < chars.len() && is_aws_class(chars[i]) {
                i += 1;
            }
            let run_len = i - start;
            if run_len == 40 {
                out.push_str(marker);
                count += 1;
            } else {
                out.extend(&chars[start..i]);
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_compiles() {
        assert_eq!(regex_floor().len(), 17);
    }

    #[test]
    fn aws_secret_key_matches_exactly_40_runs() {
        let forty = "A".repeat(40);
        let (out, n) = redact_aws_secret_key(&forty, "[REDACTED:aws_secret_key]");
        assert_eq!(n, 1);
        assert_eq!(out, "[REDACTED:aws_secret_key]");

        // 39 and 41 runs are left intact.
        for len in [39usize, 41] {
            let s = "B".repeat(len);
            let (out, n) = redact_aws_secret_key(&s, "X");
            assert_eq!(n, 0, "run of {len} must not match");
            assert_eq!(out, s);
        }
    }
}
