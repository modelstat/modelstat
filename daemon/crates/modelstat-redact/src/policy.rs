//! The `policies` config kind — a signed, ADDITIVE redaction augment layered
//! over the compiled-in floor. Port of `packages/core/src/policies.ts`.
//!
//! Hard invariant, enforced by construction: a bundle can only ADD patterns.
//! There is no field that removes, disables, or replaces the floor — the worst a
//! validly-signed bundle can do is cause MORE redaction (feature §21.6). The
//! floor in [`crate::floor`] is applied unconditionally regardless of any bundle.

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

/// The config-kind name (`@modelstat/remote-config` loader; feature §17.1).
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
}
