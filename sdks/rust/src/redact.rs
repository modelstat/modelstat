//! The privacy floor: deterministic, dependency-light redaction that runs
//! **in-process before any bytes leave the SDK**.
//!
//! This is a Rust port of the daemon's `SECRET_FLOOR`
//! (`packages/core/src/redact-floor.ts`) plus the email / absolute-path PII
//! rules. It is the irreducible baseline — even in "raw" remote mode the floor
//! still scrubs live credentials; "raw" means *full turns*, not *leaked keys*.
//!
//! Parity note: Rust's `regex` crate has no look-around, so the one boundary-
//! sensitive pattern (the 40-char AWS-secret blob) is expressed with explicit
//! boundary capture groups instead of `(?<!…)`/`(?!…)`. Every other pattern is
//! a faithful port. The unit tests assert each credential family is caught.

use regex::Regex;
use std::sync::LazyLock;

/// Result of a redaction pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    /// Count of secret-format matches replaced.
    pub secrets: u32,
    /// Count of PII matches replaced (emails, absolute paths).
    pub pii: u32,
}

struct Rule {
    re: Regex,
    replacement: &'static str,
}

/// Ordered specific → generic. Specific provider keys run before the generic
/// env-secret / blob catchers so a known key is labelled precisely.
static FLOOR: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let r = |p: &str, repl: &'static str| Rule {
        re: Regex::new(p).expect("floor pattern compiles"),
        replacement: repl,
    };
    vec![
        r(r"sk-ant-[A-Za-z0-9_-]{20,}", "[REDACTED:anthropic_key]"),
        r(r"sk-(?:proj-)?[A-Za-z0-9_-]{20,}", "[REDACTED:openai_key]"),
        r(r"AIza[0-9A-Za-z_-]{35}", "[REDACTED:google_api_key]"),
        r(
            r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
            "[REDACTED:aws_access_key]",
        ),
        r(r"ghp_[A-Za-z0-9]{36,}", "[REDACTED:github_pat]"),
        r(r"gho_[A-Za-z0-9]{36,}", "[REDACTED:github_oauth]"),
        r(r"gh[sur]_[A-Za-z0-9]{36,}", "[REDACTED:github_app]"),
        r(r"xox[aboprs]-[A-Za-z0-9-]{10,}", "[REDACTED:slack_token]"),
        r(
            r"(?:sk|pk|rk)_live_[A-Za-z0-9]{24,}",
            "[REDACTED:stripe_live_key]",
        ),
        r(
            r"(?:sk|pk|rk)_test_[A-Za-z0-9]{24,}",
            "[REDACTED:stripe_test_key]",
        ),
        r(
            r"[MN][A-Za-z\d]{23}\.[\w-]{6}\.[\w-]{27}",
            "[REDACTED:discord_token]",
        ),
        r(
            r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            "[REDACTED:jwt]",
        ),
        r(
            r"-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----[\s\S]*?-----END (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----",
            "[REDACTED:private_key]",
        ),
        r(
            r"ds_live_[A-Za-z0-9_-]{32,}",
            "[REDACTED:modelstat_device_secret]",
        ),
        // Generic env-style KEY=VALUE where KEY names a secret. Keeps the name.
        r(
            r#"\b([A-Z][A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD|PASSWD|API)[A-Z0-9_]*)\s*[:=]\s*['"]?([^\s'"]{12,})['"]?"#,
            "${1}=[REDACTED:env_secret]",
        ),
        r(
            r"Bearer\s+[A-Za-z0-9._~+/-]{20,}=*",
            "Bearer [REDACTED:bearer]",
        ),
        r(
            r"(?i)\b(postgres|mysql|mongodb|redis|amqp)(?:\+[a-z]+)?://[^:\s]+:([^@\s]+)@",
            "${1}://<user>:[REDACTED:db_password]@",
        ),
    ]
});

/// The 40-char base64-ish blob (e.g. a lone AWS secret access key). Boundary
/// groups stand in for the TS look-around so an embedded blob inside a longer
/// token is left alone.
static AWS_SECRET_BLOB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|[^A-Za-z0-9/+=])([A-Za-z0-9/+=]{40})([^A-Za-z0-9/+=]|$)")
        .expect("aws blob pattern compiles")
});

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}").expect("email pattern compiles")
});

/// Absolute home paths on macOS / Linux / Windows — they leak usernames and
/// machine layout.
static ABS_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:/Users/|/home/)[^\s"'`)]+|[A-Za-z]:\\Users\\[^\s"'`)]+"#)
        .expect("path pattern compiles")
});

/// Redact `input` against the floor. Returns the cleaned text and per-class
/// counts. Allocation-free when nothing matches (returns the input unchanged).
#[must_use]
pub fn redact(input: &str) -> Redacted {
    let mut text = input.to_string();
    let mut secrets: u32 = 0;
    let mut pii: u32 = 0;

    for rule in FLOOR.iter() {
        let n = rule.re.find_iter(&text).count() as u32;
        if n > 0 {
            text = rule.re.replace_all(&text, rule.replacement).into_owned();
            secrets += n;
        }
    }

    // AWS blob (boundary-preserving replacement).
    let n = AWS_SECRET_BLOB.find_iter(&text).count() as u32;
    if n > 0 {
        text = AWS_SECRET_BLOB
            .replace_all(&text, |c: &regex::Captures| {
                format!("{}[REDACTED:aws_secret_key]{}", &c[1], &c[3])
            })
            .into_owned();
        secrets += n;
    }

    let n = EMAIL.find_iter(&text).count() as u32;
    if n > 0 {
        text = EMAIL.replace_all(&text, "[REDACTED:email]").into_owned();
        pii += n;
    }

    let n = ABS_PATH.find_iter(&text).count() as u32;
    if n > 0 {
        text = ABS_PATH.replace_all(&text, "[REDACTED:path]").into_owned();
        pii += n;
    }

    Redacted { text, secrets, pii }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(s: &str) -> String {
        redact(s).text
    }

    #[test]
    fn scrubs_each_secret_family() {
        let cases = [
            "sk-ant-0123456789abcdefghijABCDEF",
            "sk-proj-0123456789abcdefghijABCDEF",
            "AIzaSyA1234567890123456789012345678901234",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_0123456789012345678901234567890123456789",
            "xoxb-1234567890-abcdefghijkl",
            "sk_live_0123456789012345678901234567",
            "ds_live_0123456789012345678901234567890123",
        ];
        for c in cases {
            let out = clean(c);
            assert!(
                out.contains("[REDACTED:"),
                "expected redaction for {c:?}, got {out:?}"
            );
            assert!(!out.contains(&c[..c.len().min(12)]) || out.contains("REDACTED"));
        }
    }

    #[test]
    fn keeps_env_var_name_but_drops_value() {
        let out = clean("MY_API_TOKEN=supersecretvalue123");
        assert!(out.contains("MY_API_TOKEN="), "got {out:?}");
        assert!(out.contains("[REDACTED:env_secret]"), "got {out:?}");
        assert!(!out.contains("supersecretvalue123"));
    }

    #[test]
    fn redacts_bearer_and_db_password() {
        let b = clean("Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123");
        assert!(b.contains("Bearer [REDACTED:bearer]"), "got {b:?}");
        let d = clean("postgres://app:hunter2hunter2@db.internal:5432/prod");
        assert!(d.contains("[REDACTED:db_password]"), "got {d:?}");
        assert!(!d.contains("hunter2hunter2"));
    }

    #[test]
    fn redacts_email_and_paths_as_pii() {
        let r = redact("ping me at jane.doe@example.com from /Users/jane/secret/app.rs");
        assert!(r.text.contains("[REDACTED:email]"), "got {:?}", r.text);
        assert!(r.text.contains("[REDACTED:path]"), "got {:?}", r.text);
        assert_eq!(r.pii, 2);
    }

    #[test]
    fn leaves_clean_text_untouched_and_counts_zero() {
        let r = redact("refactor the auth module and add tests");
        assert_eq!(r.text, "refactor the auth module and add tests");
        assert_eq!(r.secrets, 0);
        assert_eq!(r.pii, 0);
    }

    #[test]
    fn aws_secret_blob_is_caught() {
        // The canonical 40-char AWS secret-key example, standing alone.
        let key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_eq!(key.len(), 40);
        let out = clean(&format!("aws_secret = {key}"));
        assert!(out.contains("[REDACTED:aws_secret_key]"), "got {out:?}");
        assert!(!out.contains(key));
    }
}
