//! Public-reference mining — a port of the `detectReferences` /
//! `detectEventReferences` half of `packages/core/src/session-metadata.ts`.
//!
//! The parsers call [`detect_event_references`] over each turn's full text and
//! stamp the result on `RawEvent.references` (an opaque `Value` passthrough on the
//! wire, per PARITY.md — the full typed `SessionMetadata` port + git-outcome
//! enrichment lands in M4). Only PUBLIC reference shapes ride this — `org/repo`,
//! PR/issue numbers, ticket keys, and the URLs that contain them — so it is safe
//! to run over un-redacted turn text (same safety class as a repo slug).

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

/// Trust ranking — higher wins when the same entity is seen twice.
fn source_rank(s: &str) -> i32 {
    match s {
        "git" => 3,
        "tool" => 2,
        "content" => 1,
        _ => 0, // "model"
    }
}

/// A git host + `org/repo`, with every branch seen.
#[derive(Debug, Clone, Serialize)]
pub struct RepoRef {
    pub host: Option<String>,
    pub slug: String,
    pub branches: Vec<String>,
    pub source: String,
}

/// A pull/merge request the session referenced.
#[derive(Debug, Clone, Serialize)]
pub struct PullRequestRef {
    pub host: Option<String>,
    pub slug: Option<String>,
    pub number: u64,
    pub url: Option<String>,
    pub source: String,
    pub confidence: f64,
}

/// An issue / ticket the session referenced.
#[derive(Debug, Clone, Serialize)]
pub struct IssueRef {
    pub provider: String,
    pub key: String,
    pub slug: Option<String>,
    pub url: Option<String>,
    pub source: String,
    pub confidence: f64,
}

/// The mutable accumulation shape the detectors emit.
#[derive(Debug, Default)]
pub struct DetectedRefs {
    pub repos: Vec<RepoRef>,
    pub pull_requests: Vec<PullRequestRef>,
    pub issues: Vec<IssueRef>,
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

// Detection patterns. `\w`/`\d`/`\b` are spelled out as ASCII to match JS
// semantics (JS `\w` is ASCII-only; Rust's is Unicode by default). `(?i)` on the
// forge URLs mirrors the JS `/…/gi` flag.
fn github_pr() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?i)https?://github\.com/([0-9A-Za-z_.\-]+)/([0-9A-Za-z_.\-]+)/pull/([0-9]+)")
}
fn gitlab_mr() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?i)https?://gitlab\.com/([0-9A-Za-z_./\-]+?)/-/merge_requests/([0-9]+)")
}
fn bitbucket_pr() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?i)https?://bitbucket\.org/([0-9A-Za-z_.\-]+)/([0-9A-Za-z_.\-]+)/pull-requests/([0-9]+)")
}
fn github_issue() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?i)https?://github\.com/([0-9A-Za-z_.\-]+)/([0-9A-Za-z_.\-]+)/issues/([0-9]+)")
}
fn gitlab_issue() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?i)https?://gitlab\.com/([0-9A-Za-z_./\-]+?)/-/issues/([0-9]+)")
}
fn linear_issue() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?i)https?://linear\.app/[0-9A-Za-z_.\-]+/issue/([A-Z][A-Z0-9]*-[0-9]+)")
}
fn jira_issue() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?i)https?://[0-9A-Za-z_.\-]+/browse/([A-Z][A-Z0-9]+-[0-9]+)")
}
/// `org/repo#123` — the GitHub shorthand, anchored on a boundary. No `i` flag.
fn slug_hash() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    // `\[` — Rust reads a bare `[` inside a class as a nested-class opener (JS
    // treats it as a literal), so it must be escaped.
    re(&C, r"(?:^|[\s(\[{<])([0-9A-Za-z_.\-]+/[0-9A-Za-z_.\-]+)#([0-9]+)(?-u:\b)")
}
/// A bare `TEAM-123` ticket key. Uppercase-anchored, no `i` flag.
fn bare_ticket() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?-u:\b)([A-Z][A-Z0-9]{1,9}-[0-9]{1,6})(?-u:\b)")
}
/// PR-cue lookbehind for the shorthand (`\b(pr|pull-request|…|merged)\s*$`).
fn pr_cue() -> &'static Regex {
    static C: OnceLock<Regex> = OnceLock::new();
    re(&C, r"(?i)(?-u:\b)(pr|pull[ -]?request|merge[ -]?request|mr|merged)\s*$")
}

fn repo_from(host: Option<String>, slug: &str, source: &str) -> RepoRef {
    RepoRef {
        host,
        slug: slug.to_string(),
        branches: Vec::new(),
        source: source.to_string(),
    }
}

/// Extract every repo / PR / issue reference from a blob of text. `source` tags
/// every reference and gates the noisier patterns (bare `TEAM-123` keys only for
/// `source == "model"`).
pub fn detect_references(text: &str, source: &str) -> DetectedRefs {
    let mut out = DetectedRefs::default();
    if text.is_empty() {
        return out;
    }

    for c in github_pr().captures_iter(text) {
        let slug = format!("{}/{}", &c[1], &c[2]);
        out.pull_requests.push(PullRequestRef {
            host: Some("github.com".into()),
            slug: Some(slug.clone()),
            number: c[3].parse().unwrap_or(0),
            url: Some(c[0].to_string()),
            source: source.into(),
            confidence: 0.95,
        });
        out.repos.push(repo_from(Some("github.com".into()), &slug, source));
    }
    for c in gitlab_mr().captures_iter(text) {
        out.pull_requests.push(PullRequestRef {
            host: Some("gitlab.com".into()),
            slug: Some(c[1].to_string()),
            number: c[2].parse().unwrap_or(0),
            url: Some(c[0].to_string()),
            source: source.into(),
            confidence: 0.95,
        });
        out.repos.push(repo_from(Some("gitlab.com".into()), &c[1], source));
    }
    for c in bitbucket_pr().captures_iter(text) {
        let slug = format!("{}/{}", &c[1], &c[2]);
        out.pull_requests.push(PullRequestRef {
            host: Some("bitbucket.org".into()),
            slug: Some(slug.clone()),
            number: c[3].parse().unwrap_or(0),
            url: Some(c[0].to_string()),
            source: source.into(),
            confidence: 0.95,
        });
        out.repos.push(repo_from(Some("bitbucket.org".into()), &slug, source));
    }

    for c in github_issue().captures_iter(text) {
        let slug = format!("{}/{}", &c[1], &c[2]);
        out.issues.push(IssueRef {
            provider: "github".into(),
            key: c[3].to_string(),
            slug: Some(slug.clone()),
            url: Some(c[0].to_string()),
            source: source.into(),
            confidence: 0.95,
        });
        out.repos.push(repo_from(Some("github.com".into()), &slug, source));
    }
    for c in gitlab_issue().captures_iter(text) {
        out.issues.push(IssueRef {
            provider: "gitlab".into(),
            key: c[2].to_string(),
            slug: Some(c[1].to_string()),
            url: Some(c[0].to_string()),
            source: source.into(),
            confidence: 0.95,
        });
        out.repos.push(repo_from(Some("gitlab.com".into()), &c[1], source));
    }
    for c in linear_issue().captures_iter(text) {
        out.issues.push(IssueRef {
            provider: "linear".into(),
            key: c[1].to_string(),
            slug: None,
            url: Some(c[0].to_string()),
            source: source.into(),
            confidence: 0.9,
        });
    }
    for c in jira_issue().captures_iter(text) {
        out.issues.push(IssueRef {
            provider: "jira".into(),
            key: c[1].to_string(),
            slug: None,
            url: Some(c[0].to_string()),
            source: source.into(),
            confidence: 0.9,
        });
    }

    for c in slug_hash().captures_iter(text) {
        let m0 = c.get(0).unwrap();
        let slug = c[1].to_string();
        let number: u64 = c[2].parse().unwrap_or(0);
        // The `org/repo#N` shorthand is ambiguous. Default to an issue, but an
        // adjacent PR cue ("PR org/repo#N", "merged …") disambiguates to a PR.
        let lead_start = m0.start().saturating_sub(20);
        let lead = &text[lead_start..m0.start()];
        if pr_cue().is_match(lead) {
            out.pull_requests.push(PullRequestRef {
                host: Some("github.com".into()),
                slug: Some(slug.clone()),
                number,
                url: None,
                source: source.into(),
                confidence: 0.6,
            });
            out.repos.push(repo_from(Some("github.com".into()), &slug, source));
        } else {
            out.issues.push(IssueRef {
                provider: "github".into(),
                key: c[2].to_string(),
                slug: Some(slug),
                url: None,
                source: source.into(),
                confidence: 0.55,
            });
        }
    }

    if source == "model" {
        for c in bare_ticket().captures_iter(text) {
            out.issues.push(IssueRef {
                provider: "other".into(),
                key: c[1].to_string(),
                slug: None,
                url: None,
                source: source.into(),
                confidence: 0.4,
            });
        }
    }

    out
}

fn stronger(a: &str, b: &str) -> String {
    if source_rank(a) >= source_rank(b) {
        a.to_string()
    } else {
        b.to_string()
    }
}

fn score(source: &str, confidence: f64) -> f64 {
    source_rank(source) as f64 * 2.0 + confidence
}

/// Fold detected references into one deduped, validated, capped set (the
/// EventReferences subset: no `files`). Repos dedupe by lowercased slug unioning
/// branches; PRs/issues by their natural key keeping the strongest copy.
fn dedupe(parts: DetectedRefs) -> (Vec<RepoRef>, Vec<PullRequestRef>, Vec<IssueRef>) {
    // Repos — key by lowercased slug.
    let mut repo_order: Vec<String> = Vec::new();
    let mut repos: std::collections::HashMap<String, RepoRef> = std::collections::HashMap::new();
    for r in parts.repos {
        let key = r.slug.to_lowercase();
        match repos.get_mut(&key) {
            Some(existing) => {
                if existing.host.is_none() {
                    existing.host = r.host.clone();
                }
                for b in r.branches {
                    if !existing.branches.contains(&b) {
                        existing.branches.push(b);
                    }
                }
                existing.branches.truncate(50);
                existing.source = stronger(&existing.source, &r.source);
            }
            None => {
                repo_order.push(key.clone());
                repos.insert(key, r);
            }
        }
    }
    let repos: Vec<RepoRef> = repo_order
        .into_iter()
        .filter_map(|k| repos.remove(&k))
        .filter(valid_repo)
        .take(50)
        .collect();

    // PRs — key by `<slug>#<number>`.
    let mut pr_order: Vec<String> = Vec::new();
    let mut prs: std::collections::HashMap<String, PullRequestRef> =
        std::collections::HashMap::new();
    for p in parts.pull_requests {
        let key = format!("{}#{}", p.slug.clone().unwrap_or_default().to_lowercase(), p.number);
        match prs.get_mut(&key) {
            Some(existing) => {
                let (win, lose) = if score(&existing.source, existing.confidence)
                    >= score(&p.source, p.confidence)
                {
                    (existing.clone(), p)
                } else {
                    (p, existing.clone())
                };
                *existing = PullRequestRef {
                    host: win.host.or(lose.host),
                    slug: win.slug.or(lose.slug),
                    number: win.number,
                    url: win.url.or(lose.url),
                    source: win.source,
                    confidence: win.confidence.max(lose.confidence),
                };
            }
            None => {
                pr_order.push(key.clone());
                prs.insert(key, p);
            }
        }
    }
    let pull_requests: Vec<PullRequestRef> = pr_order
        .iter()
        .filter_map(|k| prs.remove(k))
        .filter(valid_pr)
        .take(100)
        .collect();

    // Issues — key by `<provider>:<slug>#<key>`.
    let mut issue_order: Vec<String> = Vec::new();
    let mut issues: std::collections::HashMap<String, IssueRef> = std::collections::HashMap::new();
    for i in parts.issues {
        let key = format!(
            "{}:{}#{}",
            i.provider,
            i.slug.clone().unwrap_or_default().to_lowercase(),
            i.key.to_lowercase()
        );
        match issues.get_mut(&key) {
            Some(existing) => {
                let (win, lose) = if score(&existing.source, existing.confidence)
                    >= score(&i.source, i.confidence)
                {
                    (existing.clone(), i)
                } else {
                    (i, existing.clone())
                };
                *existing = IssueRef {
                    provider: win.provider,
                    key: win.key,
                    slug: win.slug.or(lose.slug),
                    url: win.url.or(lose.url),
                    source: win.source,
                    confidence: win.confidence.max(lose.confidence),
                };
            }
            None => {
                issue_order.push(key.clone());
                issues.insert(key, i);
            }
        }
    }
    let mut deduped_issues: Vec<IssueRef> =
        issue_order.iter().filter_map(|k| issues.remove(k)).collect();

    // Reconcile the ambiguous `org/repo#N` shorthand: drop a phantom GitHub issue
    // when a real PR for the same (slug, number) exists.
    let pr_keys: std::collections::HashSet<String> = pull_requests
        .iter()
        .map(|p| format!("{}#{}", p.slug.clone().unwrap_or_default().to_lowercase(), p.number))
        .collect();
    deduped_issues.retain(|i| {
        !(i.provider == "github"
            && pr_keys.contains(&format!(
                "{}#{}",
                i.slug.clone().unwrap_or_default().to_lowercase(),
                i.key
            )))
    });
    let issues: Vec<IssueRef> = deduped_issues.into_iter().filter(valid_issue).take(100).collect();

    (repos, pull_requests, issues)
}

fn chars(s: &str) -> usize {
    s.chars().count()
}
fn opt_chars(s: &Option<String>) -> usize {
    s.as_ref().map(|v| chars(v)).unwrap_or(0)
}

fn valid_repo(r: &RepoRef) -> bool {
    opt_chars(&r.host) <= 80
        && !r.slug.is_empty()
        && chars(&r.slug) <= 200
        && r.branches.len() <= 50
        && r.branches.iter().all(|b| chars(b) <= 200)
}
fn valid_pr(p: &PullRequestRef) -> bool {
    p.number >= 1
        && opt_chars(&p.host) <= 80
        && opt_chars(&p.slug) <= 200
        && opt_chars(&p.url) <= 400
}
fn valid_issue(i: &IssueRef) -> bool {
    matches!(
        i.provider.as_str(),
        "github" | "gitlab" | "bitbucket" | "linear" | "jira" | "other"
    ) && !i.key.is_empty()
        && chars(&i.key) <= 80
        && opt_chars(&i.slug) <= 200
        && opt_chars(&i.url) <= 400
}

/// Extract + dedupe the public references in ONE event's full text. Returns None
/// when none are found, so the caller leaves `RawEvent.references` off. Bare
/// `TEAM-123` keys are deliberately NOT mined here (gated to the model channel).
pub fn detect_event_references(text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    let (repos, pull_requests, issues) = dedupe(detect_references(text, "content"));
    if repos.is_empty() && pull_requests.is_empty() && issues.is_empty() {
        return None;
    }
    let repos: Vec<RepoRef> = repos.into_iter().take(24).collect();
    let pull_requests: Vec<PullRequestRef> = pull_requests.into_iter().take(24).collect();
    let issues: Vec<IssueRef> = issues.into_iter().take(24).collect();
    Some(json!({
        "repos": repos,
        "pull_requests": pull_requests,
        "issues": issues,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_none() {
        assert!(detect_event_references("").is_none());
        assert!(detect_event_references("just some plain prose with no refs").is_none());
    }

    #[test]
    fn github_pr_url_yields_pr_and_repo() {
        let refs = detect_event_references("see https://github.com/acme/api/pull/42 please").unwrap();
        assert_eq!(refs["pull_requests"][0]["number"], 42);
        assert_eq!(refs["pull_requests"][0]["slug"], "acme/api");
        assert_eq!(refs["repos"][0]["slug"], "acme/api");
        assert_eq!(refs["repos"][0]["host"], "github.com");
    }

    #[test]
    fn shorthand_defaults_to_issue_pr_cue_flips_it() {
        let issue = detect_event_references("fixes acme/api#7").unwrap();
        assert_eq!(issue["issues"][0]["key"], "7");
        assert_eq!(issue["issues"][0]["provider"], "github");
        assert!(issue["pull_requests"].as_array().unwrap().is_empty());

        let pr = detect_event_references("merged acme/api#7").unwrap();
        assert_eq!(pr["pull_requests"][0]["number"], 7);
        // The phantom issue is reconciled away (same slug+number).
        assert!(pr["issues"].as_array().unwrap().is_empty());
    }

    #[test]
    fn linear_url_is_detected() {
        let refs = detect_event_references("https://linear.app/team/issue/ENG-742").unwrap();
        assert_eq!(refs["issues"][0]["provider"], "linear");
        assert_eq!(refs["issues"][0]["key"], "ENG-742");
    }

    #[test]
    fn bare_ticket_not_mined_in_content() {
        // UTF-8 must NOT be mined as a ticket from content text.
        assert!(detect_event_references("we use UTF-8 here").is_none());
    }
}
