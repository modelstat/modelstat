//! Git context helpers — a port of `packages/parsers/src/git.ts`.
//!
//! This module covers the pure, deterministic helpers the parsers use at parse
//! time: the path→slug heuristic and the worktree-collapsing repo-root walk. The
//! `git`-subprocess `resolveGitContext` (authoritative slug/branch) is enrichment
//! (feature §7.4) and lands with the M4 git-enrichment sub-piece.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use modelstat_wire::GitContext;
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
    let ssh =
        SSH.get_or_init(|| Regex::new(r"^(?:git@)?([^:]+):([^/]+)/([^.]+?)(?:\.git)?$").unwrap());
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
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
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

/// Run `git` in `cwd`, returning stdout on a zero exit within `timeout`, else
/// None. Thin wrapper over [`crate::util::run_command`] — git enrichment is
/// never allowed to block or fail a scan.
pub(crate) fn run_git(args: &[&str], cwd: &str, timeout: Duration) -> Option<String> {
    crate::util::run_command("git", args, Some(cwd), timeout)
}

/// Resolves authoritative git context per cwd, caching for the process lifetime
/// (a byte-faithful model of the TS module-level `cache` Map — worktrees are
/// collapsed to the main repo so every session of a repo yields the one
/// canonical remote slug). 2s git timeouts.
#[derive(Default)]
pub struct GitResolver {
    cache: HashMap<String, GitContext>,
}

impl GitResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// The git context for `cwd`, or None when `cwd` is None. A non-repo cwd
    /// resolves to an all-null context (cached), matching the TS.
    pub fn resolve(&mut self, cwd: Option<&str>) -> Option<GitContext> {
        let cwd = cwd?;
        let target = main_repo_path(cwd).to_string();
        if let Some(hit) = self.cache.get(&target) {
            return Some(hit.clone());
        }
        let ctx = match find_repo_root(&target) {
            None => GitContext {
                remote_url: None,
                remote_host: None,
                remote_slug: None,
                branch: None,
            },
            Some(root) => {
                let ran = |args: &[&str]| -> Option<String> {
                    run_git(args, &root, Duration::from_millis(2_000)).and_then(|s| {
                        let t = s.trim().to_string();
                        if t.is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    })
                };
                let remote_url = ran(&["config", "--get", "remote.origin.url"]);
                let branch = ran(&["rev-parse", "--abbrev-ref", "HEAD"]);
                let (host, slug) = match &remote_url {
                    Some(u) => parse_remote(u),
                    None => (None, None),
                };
                GitContext {
                    remote_url,
                    remote_host: host,
                    remote_slug: slug,
                    branch,
                }
            }
        };
        self.cache.insert(target, ctx.clone());
        Some(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn slug_heuristic() {
        assert_eq!(
            guess_repo_slug_from_path(Some("/Users/dev/projects/acme/myrepo")).as_deref(),
            Some("acme/myrepo")
        );
        // Single segment after `projects` → no match (claude_basic case).
        assert_eq!(
            guess_repo_slug_from_path(Some("/Users/dev/projects/myrepo")),
            None
        );
        // Worktree noise collapses to the org.
        assert_eq!(
            guess_repo_slug_from_path(Some("/home/x/src/acme/.claude")).as_deref(),
            Some("acme")
        );
        assert_eq!(guess_repo_slug_from_path(None), None);
    }

    #[test]
    fn main_repo_path_strips_worktree() {
        assert_eq!(main_repo_path("/repo/.claude/worktrees/abc"), "/repo");
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

    #[test]
    #[cfg(unix)]
    fn resolve_reads_remote_from_a_real_repo() {
        let dir = std::env::temp_dir().join(format!("modelstat-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_str().unwrap().to_string();
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        };
        // Skip cleanly if git isn't available on this runner.
        if run(&["init", "-q"]).map(|s| !s.success()).unwrap_or(true) {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let _ = run(&[
            "config",
            "remote.origin.url",
            "git@github.com:acme/myrepo.git",
        ]);
        let mut resolver = GitResolver::new();
        let ctx = resolver.resolve(Some(&path)).unwrap();
        assert_eq!(ctx.remote_slug.as_deref(), Some("acme/myrepo"));
        assert_eq!(ctx.remote_host.as_deref(), Some("github.com"));
        // Second call is served from cache.
        assert_eq!(
            resolver
                .resolve(Some(&path))
                .unwrap()
                .remote_slug
                .as_deref(),
            Some("acme/myrepo")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
