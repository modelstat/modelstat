//! Public-reference mining + the pure session-metadata core — a port of
//! `packages/core/src/session-metadata.ts` in full.
//!
//! The parsers call [`detect_event_references`] over each turn's full text and
//! stamp the result on `RawEvent.references` (an opaque `Value` passthrough on the
//! wire, per PARITY.md). Only PUBLIC reference shapes ride this — `org/repo`,
//! PR/issue numbers, ticket keys, and the URLs that contain them — so it is safe
//! to run over un-redacted turn text (same safety class as a repo slug).
//!
//! The rest of the module is the pure fold that the M4 session-metadata pass
//! ([`crate::git_enrich`] + `modelstat-pipeline`) drives: the typed
//! [`SessionMetadata`] / [`FileRef`], the branch-ticket miner
//! ([`detect_branch_tickets`]), and the deduplicators ([`dedupe_session_metadata`],
//! [`dedupe_files`]). Everything here is deterministic + I/O-free; the git + model
//! channels are injected by the pass.

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ── Zod-parity defaults (used when deserializing a `RawEvent.references` blob or
//    a replayed reference back into the typed shape). ──────────────────────────
fn default_content_source() -> String {
    "content".to_string()
}
fn default_git_source() -> String {
    "git".to_string()
}
fn default_pr_confidence() -> f64 {
    0.9
}
fn default_issue_confidence() -> f64 {
    0.8
}
fn default_issue_provider() -> String {
    "other".to_string()
}

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoRef {
    #[serde(default)]
    pub host: Option<String>,
    pub slug: String,
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default = "default_content_source")]
    pub source: String,
}

/// A pull/merge request the session referenced. The `merged*`/`reverted` fields
/// are the on-device verified-outcome signals filled by the pass's local
/// git-check (`checkPullRequestOutcome`); they are `.optional()` in the TS schema,
/// so they are OMITTED (not `null`) until enrichment sets them. `merged_at` is
/// additionally nullable — an enriched-but-unmerged PR serializes it as `null` —
/// hence the nested `Option<Option<…>>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestRef {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    pub number: u64,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default = "default_content_source")]
    pub source: String,
    #[serde(default = "default_pr_confidence")]
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverted: Option<bool>,
}

/// An issue / ticket the session referenced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueRef {
    #[serde(default = "default_issue_provider")]
    pub provider: String,
    pub key: String,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default = "default_content_source")]
    pub source: String,
    #[serde(default = "default_issue_confidence")]
    pub confidence: f64,
}

/// A file a session changed, with the lines added/deleted across the session's
/// window (git `--numstat`). Always `git`-sourced + repo-relative — the same
/// public-shape safety class as a slug (no contents, no home paths).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRef {
    #[serde(default)]
    pub slug: Option<String>,
    pub path: String,
    #[serde(default)]
    pub lines_added: u64,
    #[serde(default)]
    pub lines_deleted: u64,
    #[serde(default = "default_git_source")]
    pub source: String,
}

/// The deterministic metadata for one session — attached to the ingest batch
/// under `session_metadata[session_id]`. Every collection is plural + capped; an
/// empty one ([`is_empty_session_metadata`]) is simply not shipped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetadata {
    #[serde(default)]
    pub repos: Vec<RepoRef>,
    #[serde(default)]
    pub pull_requests: Vec<PullRequestRef>,
    #[serde(default)]
    pub issues: Vec<IssueRef>,
    #[serde(default)]
    pub files: Vec<FileRef>,
}

/// The mutable accumulation shape the detectors emit. Deserialize is how a
/// `RawEvent.references` blob (the per-event `{repos,pull_requests,issues}`) is
/// folded back into the session-level dedupe.
#[derive(Debug, Default, Deserialize)]
pub struct DetectedRefs {
    #[serde(default)]
    pub repos: Vec<RepoRef>,
    #[serde(default)]
    pub pull_requests: Vec<PullRequestRef>,
    #[serde(default)]
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
            merged: None,
            merged_at: None,
            reverted: None,
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
            merged: None,
            merged_at: None,
            reverted: None,
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
            merged: None,
            merged_at: None,
            reverted: None,
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
                merged: None,
                merged_at: None,
                reverted: None,
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
                    // Outcome signals are filled AFTER dedupe (like the TS merge,
                    // which returns only the six core fields), so reset to unknown.
                    merged: None,
                    merged_at: None,
                    reverted: None,
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

/// Mine ticket keys (`TEAM-123`) out of a branch name — a high-signal, low-noise
/// place to find them (`feature/ENG-742-retry-logic`). Returns `git`-sourced issue
/// refs (a branch is deterministic, not content). Port of `detectBranchTickets`.
pub fn detect_branch_tickets(branch: Option<&str>) -> Vec<IssueRef> {
    let mut out = Vec::new();
    let Some(branch) = branch.filter(|b| !b.is_empty()) else {
        return out;
    };
    for c in bare_ticket().captures_iter(branch) {
        out.push(IssueRef {
            provider: "other".into(),
            key: c[1].to_string(),
            slug: None,
            url: None,
            source: "git".into(),
            confidence: 0.7,
        });
    }
    out
}

/// Fold any number of [`DetectedRefs`] (from every channel + every event/segment
/// of a session) into one validated, deduped, capped [`SessionMetadata`]. Reuses
/// the exact per-field [`dedupe`] core (caps 50/100/100, reconcile, keep-valid);
/// `files` is left empty — the pass fills it after dedupe via [`dedupe_files`].
/// Port of `dedupeSessionMetadata`.
pub fn dedupe_session_metadata(parts: Vec<DetectedRefs>) -> SessionMetadata {
    let mut all = DetectedRefs::default();
    for p in parts {
        all.repos.extend(p.repos);
        all.pull_requests.extend(p.pull_requests);
        all.issues.extend(p.issues);
    }
    let (repos, pull_requests, issues) = dedupe(all);
    SessionMetadata {
        repos,
        pull_requests,
        issues,
        files: Vec::new(),
    }
}

fn valid_file(f: &FileRef) -> bool {
    chars(&f.path) <= 400 && opt_chars(&f.slug) <= 200
}

/// Fold a session's [`FileRef`]s (collected per repo from git) into one deduped,
/// capped list: the same `(slug, path)` sums its line counts and keeps the
/// strongest source. Preserves first-seen order (JS `Map`). Port of `dedupeFiles`.
pub fn dedupe_files(files: Vec<FileRef>) -> Vec<FileRef> {
    let mut order: Vec<String> = Vec::new();
    let mut by_key: std::collections::HashMap<String, FileRef> = std::collections::HashMap::new();
    for f in files {
        let key = format!("{}:{}", f.slug.clone().unwrap_or_default().to_lowercase(), f.path);
        match by_key.get_mut(&key) {
            Some(e) => {
                if e.slug.is_none() {
                    e.slug = f.slug.clone();
                }
                e.lines_added += f.lines_added;
                e.lines_deleted += f.lines_deleted;
                e.source = stronger(&e.source, &f.source);
            }
            None => {
                order.push(key.clone());
                by_key.insert(key, f);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|k| by_key.remove(&k))
        .filter(valid_file)
        .take(500)
        .collect()
}

/// True when a [`SessionMetadata`] carries no references — the pass uses this to
/// avoid shipping (and overwriting server state with) an empty map.
pub fn is_empty_session_metadata(m: &SessionMetadata) -> bool {
    m.repos.is_empty() && m.pull_requests.is_empty() && m.issues.is_empty() && m.files.is_empty()
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

    #[test]
    fn branch_tickets_are_git_sourced() {
        let issues = detect_branch_tickets(Some("feature/ENG-742-retry-logic"));
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].key, "ENG-742");
        assert_eq!(issues[0].provider, "other");
        assert_eq!(issues[0].source, "git");
        assert_eq!(issues[0].confidence, 0.7);
        // None / empty / ticket-free branches yield nothing.
        assert!(detect_branch_tickets(None).is_empty());
        assert!(detect_branch_tickets(Some("")).is_empty());
        assert!(detect_branch_tickets(Some("main")).is_empty());
    }

    #[test]
    fn dedupe_session_metadata_unions_repos_and_ranks_sources() {
        // Same repo seen from a weak content mention and a strong git context:
        // branches union, git source wins, one repo survives.
        let content = detect_references("touched acme/api#7", "content");
        let mut git = DetectedRefs::default();
        git.repos.push(RepoRef {
            host: Some("github.com".into()),
            slug: "acme/api".into(),
            branches: vec!["main".into()],
            source: "git".into(),
        });
        let meta = dedupe_session_metadata(vec![content, git]);
        assert_eq!(meta.repos.len(), 1);
        assert_eq!(meta.repos[0].slug, "acme/api");
        assert_eq!(meta.repos[0].source, "git");
        assert_eq!(meta.repos[0].branches, vec!["main".to_string()]);
        assert!(meta.files.is_empty());
    }

    #[test]
    fn event_references_blob_round_trips_into_detected_refs() {
        // The opaque `RawEvent.references` Value folds back into the typed shape
        // (the pass's channel 3a). Outcome fields are absent → deserialize None.
        let blob = detect_event_references("see https://github.com/acme/api/pull/42").unwrap();
        let refs: DetectedRefs = serde_json::from_value(blob).unwrap();
        assert_eq!(refs.pull_requests.len(), 1);
        assert_eq!(refs.pull_requests[0].number, 42);
        assert_eq!(refs.pull_requests[0].slug.as_deref(), Some("acme/api"));
        assert!(refs.pull_requests[0].merged.is_none());
    }

    #[test]
    fn pr_outcome_fields_serialize_only_once_enriched() {
        let mut pr = PullRequestRef {
            host: Some("github.com".into()),
            slug: Some("acme/api".into()),
            number: 42,
            url: None,
            source: "git".into(),
            confidence: 0.95,
            merged: None,
            merged_at: None,
            reverted: None,
        };
        // Unenriched: the three keys are omitted entirely.
        let bare = serde_json::to_value(&pr).unwrap();
        assert!(bare.get("merged").is_none());
        assert!(bare.get("merged_at").is_none());
        // Enriched-but-unmerged: keys present, merged_at is explicit null.
        pr.merged = Some(false);
        pr.merged_at = Some(None);
        pr.reverted = Some(false);
        let enriched = serde_json::to_value(&pr).unwrap();
        assert_eq!(enriched["merged"], serde_json::json!(false));
        assert_eq!(enriched["merged_at"], serde_json::Value::Null);
        assert!(enriched.get("merged_at").is_some());
    }

    #[test]
    fn dedupe_files_sums_lines_and_keeps_first_seen_order() {
        let files = vec![
            FileRef {
                slug: Some("acme/api".into()),
                path: "src/a.ts".into(),
                lines_added: 3,
                lines_deleted: 1,
                source: "git".into(),
            },
            FileRef {
                slug: Some("acme/api".into()),
                path: "src/b.ts".into(),
                lines_added: 1,
                lines_deleted: 0,
                source: "git".into(),
            },
            FileRef {
                slug: Some("acme/api".into()),
                path: "src/a.ts".into(),
                lines_added: 2,
                lines_deleted: 4,
                source: "git".into(),
            },
        ];
        let out = dedupe_files(files);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].path, "src/a.ts");
        assert_eq!(out[0].lines_added, 5);
        assert_eq!(out[0].lines_deleted, 5);
        assert_eq!(out[1].path, "src/b.ts");
    }

    #[test]
    fn empty_metadata_is_flagged() {
        assert!(is_empty_session_metadata(&SessionMetadata::default()));
        let mut m = SessionMetadata::default();
        m.files.push(FileRef {
            slug: None,
            path: "x".into(),
            lines_added: 0,
            lines_deleted: 0,
            source: "git".into(),
        });
        assert!(!is_empty_session_metadata(&m));
    }
}
