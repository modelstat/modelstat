//! The `policies` config kind — an ADDITIVE redaction augment layered over the
//! compiled-in floor. Port of `packages/core/src/policies.ts`.
//!
//! Hard invariant, enforced by construction: a bundle can only ADD patterns.
//! There is no field that removes, disables, or replaces the floor — the worst a
//! bundle from the server can do is cause MORE redaction (feature §21.6). The
//! floor in [`crate::floor`] is applied unconditionally regardless of any bundle.
//!
//! Trust is the TLS connection to the configured api origin, nothing more: the
//! payload is pure data, and the only authority it has is "redact this too".
//! That is why it needs no signature — see `modelstat_ingest::remote_config`.

use std::sync::{Arc, OnceLock, RwLock};

use regex::Regex;
use serde::{Deserialize, Serialize};

/// One additive secret pattern from a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionPattern {
    /// Stable label for the `[REDACTED:name]` placeholder. Zod: `min(1).max(64)`
    /// matching `^[a-z0-9_]+$`.
    pub name: String,
    /// JS regex source. Zod: `min(1).max(1000)`. Compiled with `g` on the client
    /// (implicit in Rust's `replace_all`).
    pub regex: String,
    /// Optional extra flags; `g` is always added. Zod: `max(8)` matching
    /// `^[imsu]*$`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<String>,
}

impl RedactionPattern {
    /// Validate against the Zod constraints (used when accepting a bundle).
    pub fn is_valid(&self) -> bool {
        let name_ok = (1..=64).contains(&self.name.len())
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        let regex_ok = (1..=1000).contains(&self.regex.len());
        let flags_ok = match &self.flags {
            None => true,
            Some(f) => f.len() <= 8 && f.chars().all(|c| matches!(c, 'i' | 'm' | 's' | 'u')),
        };
        name_ok && regex_ok && flags_ok
    }
}

/// A verified bundle: additive patterns unioned on top of the floor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionPolicyBundle {
    pub version: u64,
    pub patterns: Vec<RedactionPattern>,
}

/// The bundled fallback — no augment (the floor alone applies until a signed
/// bundle is verified).
pub const POLICIES_BUNDLED_FALLBACK: RedactionPolicyBundle = RedactionPolicyBundle {
    version: 0,
    patterns: Vec::new(),
};

/// The config-kind name — `GET /v1/config/policies` (feature §17.1).
pub const POLICIES_CONFIG_KIND: &str = "policies";

/// A compiled additive pattern ready to run in [`crate::redact_with_remote`].
pub struct CompiledPattern {
    pub name: String,
    pub re: Regex,
}

/// Compile a verified bundle's patterns. Invalid regexes / entries are SKIPPED,
/// never fatal — a bad remote pattern must not take down redaction. Port of
/// `compilePolicyPatterns`.
pub fn compile_policy_patterns(bundle: &RedactionPolicyBundle) -> Vec<CompiledPattern> {
    let mut out = Vec::new();
    for p in &bundle.patterns {
        if !p.is_valid() {
            continue;
        }
        // JS builds `new RegExp(regex, "g"+flags)`. `g` is implicit here; the
        // i/m/s flags map to inline flags. `u` is Rust's default (Unicode on).
        let mut inline = String::new();
        if let Some(f) = &p.flags {
            for c in f.chars() {
                if matches!(c, 'i' | 'm' | 's') && !inline.contains(c) {
                    inline.push(c);
                }
            }
        }
        let src = if inline.is_empty() {
            p.regex.clone()
        } else {
            format!("(?{inline}){}", p.regex)
        };
        if let Ok(re) = Regex::new(&src) {
            out.push(CompiledPattern {
                name: p.name.clone(),
                re,
            });
        }
    }
    out
}

// ── The process-wide augment ─────────────────────────────────────────────────

/// The additive set THIS PROCESS redacts with, held next to the floor it
/// augments rather than threaded through every caller.
///
/// Threading it would be the same value passed down a dozen call chains
/// (parsers, the cloud flush, abstracts, script summaries, tool commands), and
/// the first site that forgot the argument would ship text scrubbed by less than
/// the policy says — a silent weakening, discoverable only by reading the
/// leaked bytes. The floor is already process-wide state ([`crate::floor`]); the
/// augment is part of the same commitment, so it lives in the same place and
/// every floor call gets it for free.
///
/// Safe as a global precisely because it is ADDITIVE: whatever is installed, the
/// floor still runs first and unconditionally, so the worst a wrong value can do
/// is redact more than needed.
fn installed() -> &'static RwLock<Arc<Vec<CompiledPattern>>> {
    static CELL: OnceLock<RwLock<Arc<Vec<CompiledPattern>>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(Arc::new(Vec::new())))
}

/// Install a compiled bundle as this process's augment. The refresh loop calls
/// this every time a newer bundle lands; in-flight redactions keep the snapshot
/// they started with and the next one sees the new set (the same swap semantics
/// the daemon's model handles use).
pub fn install_policy_patterns(patterns: Vec<CompiledPattern>) {
    // Poison-safe: a panicked writer must not wedge every future redaction.
    *installed().write().unwrap_or_else(|e| e.into_inner()) = Arc::new(patterns);
}

/// This process's augment. Cheap (a read lock long enough to clone an `Arc`),
/// which is what lets [`crate::redact`] consult it per call.
pub fn installed_policy_patterns() -> Arc<Vec<CompiledPattern>> {
    installed()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_empty_v0() {
        assert_eq!(POLICIES_BUNDLED_FALLBACK.version, 0);
        assert!(POLICIES_BUNDLED_FALLBACK.patterns.is_empty());
    }

    #[test]
    fn bundle_roundtrips_json() {
        let json = r#"{"version":3,"patterns":[{"name":"acme_key","regex":"acme_[a-z0-9]{20,}"}]}"#;
        let bundle: RedactionPolicyBundle = serde_json::from_str(json).unwrap();
        assert_eq!(bundle.version, 3);
        assert_eq!(bundle.patterns.len(), 1);
        let compiled = compile_policy_patterns(&bundle);
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].name, "acme_key");
    }

    #[test]
    fn invalid_entries_are_skipped_not_fatal() {
        let bundle = RedactionPolicyBundle {
            version: 1,
            patterns: vec![
                RedactionPattern {
                    name: "BadName".into(), // uppercase → invalid
                    regex: "x+".into(),
                    flags: None,
                },
                RedactionPattern {
                    name: "ok".into(),
                    regex: "(unclosed".into(), // won't compile
                    flags: None,
                },
                RedactionPattern {
                    name: "good".into(),
                    regex: "z+".into(),
                    flags: None,
                },
            ],
        };
        let compiled = compile_policy_patterns(&bundle);
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].name, "good");
    }

    // ── The additive-only proof ──────────────────────────────────────────────
    // A bundle is data from the server. These tests are the standing evidence
    // that the worst it can do is cause MORE redaction — that no payload,
    // however hostile, can put a secret back on the wire.

    /// A synthetic key that the compiled floor catches on its own. Split so the
    /// committed source matches no secret scanner.
    fn floored_secret() -> String {
        concat!("use sk-ant-", "api03-abcdefghijklmnopqrstuvwxyz0123456789").to_string()
    }

    fn bundle(patterns: Vec<RedactionPattern>) -> RedactionPolicyBundle {
        RedactionPolicyBundle {
            version: 99,
            patterns,
        }
    }

    fn pattern(name: &str, regex: &str) -> RedactionPattern {
        RedactionPattern {
            name: name.into(),
            regex: regex.into(),
            flags: None,
        }
    }

    #[test]
    fn no_bundle_can_weaken_the_floor() {
        let secret = floored_secret();
        // Every shape of "make the floor stop working" a payload could attempt:
        // say nothing; try to match the whole text away; try to rewrite the
        // placeholder the floor just wrote; claim the floor's own pattern name.
        for hostile in [
            vec![],
            vec![pattern("swallow_everything", "[\\s\\S]*")],
            vec![pattern("unredact", "\\[REDACTED:anthropic_key\\]")],
            vec![pattern("anthropic_key", "nothing_of_the_sort")],
        ] {
            let compiled = compile_policy_patterns(&bundle(hostile));
            let out = crate::redact_with_remote(&secret, None, &compiled);
            assert!(
                !out.text.contains("sk-ant-"),
                "the floor must still fire: {}",
                out.text
            );
            assert!(out.counts.secrets_found >= 1);
        }
    }

    #[test]
    fn a_bundle_only_ever_adds() {
        // Text the floor alone leaves untouched, plus a pattern for it.
        let text = "deploy token acme_0123456789abcdefghij";
        let plain = crate::redact_with_remote(text, None, &[]);
        assert_eq!(plain.text, text);
        assert_eq!(plain.counts.secrets_found, 0);

        let compiled =
            compile_policy_patterns(&bundle(vec![pattern("acme_token", "acme_[a-z0-9]{20}")]));
        let augmented = crate::redact_with_remote(text, None, &compiled);
        assert!(augmented.text.contains("[REDACTED:acme_token]"));
        assert_eq!(augmented.counts.secrets_found, 1);
    }

    #[test]
    fn the_installed_set_is_what_redact_applies() {
        let _g = crate::test_policy_lock();
        // The installed set is process-wide, and this crate's other tests call
        // `redact()` on their own threads. A probe pattern that matches nothing
        // but this test's own text keeps that harmless — which is itself the
        // additive property under test.
        let text = "probe zzprobe_000111222333";
        install_policy_patterns(Vec::new());
        assert_eq!(crate::redact(text, None).text, text);

        install_policy_patterns(compile_policy_patterns(&bundle(vec![pattern(
            "zzprobe",
            "zzprobe_[0-9]{12}",
        )])));
        assert!(crate::redact(text, None)
            .text
            .contains("[REDACTED:zzprobe]"));
        // The floor is untouched by having an augment installed.
        assert!(!crate::redact(&floored_secret(), None)
            .text
            .contains("sk-ant-"));

        // …and uninstalling leaves the floor exactly as it was.
        install_policy_patterns(Vec::new());
        assert_eq!(crate::redact(text, None).text, text);
        assert!(!crate::redact(&floored_secret(), None)
            .text
            .contains("sk-ant-"));
    }
}
