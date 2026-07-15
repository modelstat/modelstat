//! Git context helpers — a port of `packages/parsers/src/git.ts`.
//!
//! This module covers the pure, deterministic helpers the parsers use at parse
//! time: the path→slug heuristic and the worktree-collapsing repo-root walk. The
//! `git`-subprocess `resolveGitContext` (authoritative slug/branch) is enrichment
//! (feature §7.4) and lands with the M4 git-enrichment sub-piece.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

/// The main-repo path for a (possibly ephemeral) worktree cwd: strips
/// `/.claude/…` so a deleted worktree still resolves the real repo. A plain cwd
/// is returned unchanged.
pub fn main_repo_path(cwd: &str) -> &str {
    match cwd.find("/.claude/") {
        Some(i) => &cwd[..i],
        None => cwd,
    }
}

/// The repo-root directory for a cwd — the nearest `.git` (worktrees collapsed to
/// the main repo via [`main_repo_path`]), or None when none is reachable.
/// Deterministic + sync (an `exists` walk).
pub fn resolve_repo_root(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?;
    find_repo_root(main_repo_path(cwd))
}

/// Walk up at most 10 parents from `start_cwd` looking for a `.git` entry.
fn find_repo_root(start_cwd: &str) -> Option<String> {
    // `path::resolve` in Node makes the path absolute against process cwd; here
    // the daemon always hands us absolute cwds, so we normalize lightly.
    let mut cur = Path::new(start_cwd).to_path_buf();
    for _ in 0..10 {
        if cur.join(".git").exists() {
            return Some(cur.to_string_lossy().into_owned());
        }
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent.to_path_buf(),
            _ => return None,
        }
    }
    None
}

/// Parse a git remote URL into `(host, slug)`. Handles both the
/// `git@github.com:org/repo(.git)?` SSH form and `https://…/org/repo(.git)?`.
pub fn parse_remote(url: &str) -> (Option<String>, Option<String>) {
    // git@github.com:org/repo(.git)?  OR  ([host]:)org/repo
    static SSH: OnceLock<Regex> = OnceLock::new();
    let ssh = SSH.get_or_init(|| Regex::new(r"^(?:git@)?([^:]+):([^/]+)/([^.]+?)(?:\.git)?$").unwrap());
    if let Some(c) = ssh.captures(url) {
        let host = c.get(1).map(|m| m.as_str().to_string());
        let slug = Some(format!("{}/{}", &c[2], &c[3]));
        return (host, slug);
    }
    // https://host/org/repo(.git)?/?  — parse as a URL.
    if let Some((host, path)) = split_url(url) {
        static PATH_RE: OnceLock<Regex> = OnceLock::new();
        let re = PATH_RE.get_or_init(|| Regex::new(r"^/([^/]+)/([^/]+?)(?:\.git)?/?$").unwrap());
        if let Some(c) = re.captures(&path) {
            return (Some(host), Some(format!("{}/{}", &c[1], &c[2])));
        }
        return (Some(host), None);
    }
    (None, None)
}

/// Minimal URL split for the http(s) remote case: `(host, pathname)`.
fn split_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // Strip any userinfo@ and :port from the authority.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), path.to_string()))
}

/// Synchronous best-effort path→slug derivation, used when we cannot invoke git
/// (walking many old sessions, or an ephemeral worktree deleted before parse).
/// Heuristic only — matches the TS `guessRepoSlugFromPath` regex exactly.
pub fn guess_repo_slug_from_path(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)/(?:www|src|code|repos|projects)/([^/]+)/([^/]+)").unwrap()
    });
    let c = re.captures(cwd)?;
    let a = c.get(1)?.as_str();
    let b = c.get(2)?.as_str();
    if a.is_empty() || b.is_empty() {
        return None;
    }
    // `<b>` is worktree/tooling noise (`<repo>/.claude/worktrees/<id>`), not a
    // repo name: the project is just `<a>`.
    if b.starts_with('.') || b == "worktrees" {
        return Some(a.to_string());
    }
    Some(format!("{a}/{b}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_heuristic() {
        assert_eq!(
            guess_repo_slug_from_path(Some("/Users/dev/projects/acme/myrepo")).as_deref(),
            Some("acme/myrepo")
        );
        // Single segment after `projects` → no match (claude_basic case).
        assert_eq!(guess_repo_slug_from_path(Some("/Users/dev/projects/myrepo")), None);
        // Worktree noise collapses to the org.
        assert_eq!(
            guess_repo_slug_from_path(Some("/home/x/src/acme/.claude")).as_deref(),
            Some("acme")
        );
        assert_eq!(guess_repo_slug_from_path(None), None);
    }

    #[test]
    fn main_repo_path_strips_worktree() {
        assert_eq!(
            main_repo_path("/repo/.claude/worktrees/abc"),
            "/repo"
        );
        assert_eq!(main_repo_path("/repo/src"), "/repo/src");
    }

    #[test]
    fn parse_remote_forms() {
        assert_eq!(
            parse_remote("git@github.com:acme/myrepo.git"),
            (Some("github.com".into()), Some("acme/myrepo".into()))
        );
        assert_eq!(
            parse_remote("https://github.com/acme/myrepo.git"),
            (Some("github.com".into()), Some("acme/myrepo".into()))
        );
        assert_eq!(
            parse_remote("https://gitlab.com/group/sub/repo"),
            (Some("gitlab.com".into()), None)
        );
    }
}
