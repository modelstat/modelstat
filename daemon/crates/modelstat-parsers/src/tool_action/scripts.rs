//! Detect the script/bash files a command references and resolve each to a real
//! on-disk path — a port of `packages/parsers/src/tool-action/scripts.ts`.
//!
//! Detection is pure-string; resolution takes an injectable `exists` so it is
//! testable and runtime-agnostic. A command can reference several scripts; they
//! are returned in command order so each abstract stays positionally linked.

use std::sync::OnceLock;

use regex::Regex;

/// Shell metacharacters that separate command segments (`&&`, `||`, `;`, `|`,
/// `&`, newlines) with surrounding whitespace.
fn segment_sep() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*(?:&&|\|\||[;|&\n])\s*").unwrap())
}

/// A trailing script-ish file extension — a generic structural signal.
fn script_ext() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\.(sh|bash|zsh|ksh|fish|py|rb|js|mjs|cjs|ts|tsx|pl|php|lua|ps1|bat|cmd|r|jl|groovy|kts)$").unwrap()
    })
}

/// Every script/bash file the command references, in order of appearance. Tokens
/// are exactly as they appear (pre-redaction) so the caller can resolve them on
/// disk and map them to the redacted form.
pub fn detect_script_refs(command: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for segment in segment_sep().split(command) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        for (i, tok) in tokens.iter().enumerate() {
            let t = strip_quotes(tok);
            if t.is_empty() || t.starts_with('-') || t.contains("://") {
                continue; // flags, URLs
            }
            let looks_like_script = script_ext().is_match(t)
                || t.starts_with("./")
                || t.starts_with("../")
                || (i == 0 && t.contains('/'));
            if looks_like_script {
                refs.push(t.to_string());
            }
        }
    }
    refs
}

/// Candidate paths to try for a referenced script, ordered MOST-ABSOLUTE /
/// LONGEST first (so the most specific existing file wins).
pub fn script_candidates(reference: &str, roots: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |p: String, out: &mut Vec<String>| {
        if !p.is_empty() && !out.contains(&p) {
            out.push(p);
        }
    };
    if is_absolute(reference) {
        push(reference.to_string(), &mut out);
    }
    for root in roots {
        if !root.is_empty() {
            push(join_path(root, reference), &mut out);
        }
    }
    push(reference.to_string(), &mut out); // bare, relative to the process cwd
    out.sort_by(|a, b| {
        let by_abs = (is_absolute(b) as i32).cmp(&(is_absolute(a) as i32));
        if by_abs != std::cmp::Ordering::Equal {
            by_abs
        } else {
            b.len().cmp(&a.len())
        }
    });
    out
}

/// Resolve a script ref to the first existing candidate (most-specific first),
/// or None when none exists.
pub fn resolve_script_path(
    reference: &str,
    roots: &[String],
    exists: &dyn Fn(&str) -> bool,
) -> Option<String> {
    script_candidates(reference, roots)
        .into_iter()
        .find(|cand| exists(cand))
}

fn is_absolute(p: &str) -> bool {
    if p.starts_with('/') {
        return true;
    }
    // /^[A-Za-z]:[\\/]/ — a Windows drive-letter path.
    let bytes = p.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn join_path(root: &str, rel: &str) -> String {
    let root = root.trim_end_matches('/');
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    format!("{root}/{rel}")
}

fn strip_quotes(token: &str) -> &str {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() >= 2 {
        let a = chars[0];
        let b = chars[chars.len() - 1];
        if (a == '\'' && b == '\'') || (a == '"' && b == '"') {
            let start = a.len_utf8();
            let end = token.len() - b.len_utf8();
            return &token[start..end];
        }
    }
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_relative_and_extension_refs() {
        assert_eq!(detect_script_refs("./deploy.sh --now"), vec!["./deploy.sh"]);
        assert_eq!(
            detect_script_refs("python scripts/build.py && bash ci/run.sh"),
            vec!["scripts/build.py", "ci/run.sh"]
        );
        // A leading path-to-an-executable (i==0 && contains '/').
        assert_eq!(detect_script_refs("/usr/local/bin/tool arg"), vec!["/usr/local/bin/tool"]);
        // Flags and URLs are skipped.
        assert!(detect_script_refs("curl https://example.com/x.sh").is_empty());
    }

    #[test]
    fn candidates_absolute_first_then_longest() {
        let roots = vec!["/repo".to_string(), "/repo/scripts".to_string()];
        let cands = script_candidates("build.py", &roots);
        // Both roots joined; longest first among non-absolute; bare last.
        assert_eq!(cands[0], "/repo/scripts/build.py");
        assert_eq!(cands[1], "/repo/build.py");
        assert_eq!(cands.last().unwrap(), "build.py");
    }

    #[test]
    fn resolve_picks_first_existing() {
        let roots = vec!["/a".to_string(), "/b".to_string()];
        let got = resolve_script_path("x.sh", &roots, &|p| p == "/b/x.sh");
        assert_eq!(got.as_deref(), Some("/b/x.sh"));
        let none = resolve_script_path("x.sh", &roots, &|_| false);
        assert_eq!(none, None);
    }
}
