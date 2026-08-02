//! Authoritative git-remote enrichment — a port of
//! `apps/daemon/src/git-enrich.ts`. Run over a batch of parsed events BEFORE
//! segmentation so the project identity keys on the REAL repository, not the
//! parser's `guessRepoSlugFromPath` heuristic (which mistakes a repo-internal
//! `…/src/app/x` subtree for a repo).
//!
//! Per distinct cwd, resolve in order: (1) the real `owner/repo` from
//! `remote.origin.url` (cwd-cached), else (2) the repo-ROOT directory name (a
//! bare name that can never be a subdirectory) for a repo with no remote. Only
//! when NO `.git` is reachable is the parser's guess left in place. The turn's
//! HISTORICAL branch is preserved (env inference + branch-ticket detection want
//! it); only the repo identity is corrected.

use std::collections::HashMap;

use modelstat_wire::{GitContext, RawEvent};

/// The corrected repo identity for a cwd (slug always present).
struct RepoIdentity {
    remote_url: Option<String>,
    remote_host: Option<String>,
    remote_slug: String,
    branch: Option<String>,
}

/// The last path component of `path` (`/a/b/repo` → `repo`), or `""`.
fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Correct each event's `git` to the authoritative on-disk remote. `resolve_git`
/// reads a cwd's git context (cwd-cached in the real impl); `resolve_root` walks
/// to the nearest `.git` root. Both are injected so this is unit-testable without
/// a real repo. Port of `resolveAuthoritativeGit`.
pub fn resolve_authoritative_git(
    events: &[RawEvent],
    mut resolve_git: impl FnMut(&str) -> Option<GitContext>,
    resolve_root: impl Fn(&str) -> Option<String>,
) -> Vec<RawEvent> {
    // One resolution per DISTINCT cwd, first-seen order.
    let mut cwds: Vec<&str> = Vec::new();
    for e in events {
        if let Some(c) = e.cwd.as_deref().filter(|c| !c.is_empty()) {
            if !cwds.contains(&c) {
                cwds.push(c);
            }
        }
    }
    if cwds.is_empty() {
        return events.to_vec();
    }

    let mut resolved: HashMap<String, RepoIdentity> = HashMap::new();
    for cwd in &cwds {
        let g = resolve_git(cwd);
        let slug = g
            .as_ref()
            .and_then(|x| x.remote_slug.clone())
            .filter(|s| !s.is_empty());
        if let Some(slug) = slug {
            // Authoritative: a real `owner/repo` remote.
            let g = g.expect("slug came from g");
            resolved.insert(
                cwd.to_string(),
                RepoIdentity {
                    remote_url: g.remote_url,
                    remote_host: g.remote_host,
                    remote_slug: slug,
                    branch: g.branch,
                },
            );
            continue;
        }
        // No remote → key on the repo-root directory NAME (bare, never a subpath).
        let branch_fallback = g.and_then(|x| x.branch);
        let name = resolve_root(cwd).map(|r| basename(&r)).unwrap_or_default();
        if !name.is_empty() {
            resolved.insert(
                cwd.to_string(),
                RepoIdentity {
                    remote_url: None,
                    remote_host: None,
                    remote_slug: name,
                    branch: branch_fallback,
                },
            );
        }
        // else: no `.git` reachable — leave the event's parsed value untouched.
    }
    if resolved.is_empty() {
        return events.to_vec();
    }

    events
        .iter()
        .map(|e| {
            let id = e.cwd.as_deref().and_then(|c| resolved.get(c));
            match id {
                None => e.clone(),
                Some(id) => {
                    let mut ev = e.clone();
                    ev.git = Some(GitContext {
                        remote_url: id.remote_url.clone(),
                        remote_host: id.remote_host.clone(),
                        remote_slug: Some(id.remote_slug.clone()),
                        // Keep the branch the parser recorded for THIS turn; fall
                        // back to the on-disk branch only when the event had none.
                        branch: e
                            .git
                            .as_ref()
                            .and_then(|g| g.branch.clone())
                            .or_else(|| id.branch.clone()),
                    });
                    ev
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(cwd: Option<&str>, branch: Option<&str>) -> RawEvent {
        RawEvent {
            source_event_id: "e".into(),
            ts: "2026-07-16T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s1".into(),
            turn_index: None,
            parent_event_id: None,
            cwd: cwd.map(Into::into),
            git: branch.map(|b| GitContext {
                remote_url: None,
                remote_host: None,
                remote_slug: Some("guessed/subdir".into()),
                branch: Some(b.into()),
            }),
            tokens: None,
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            pricing_mode: "subscription".to_string(),
        }
    }

    #[test]
    fn a_real_remote_overrides_the_path_guess_and_keeps_the_turn_branch() {
        let events = vec![ev(Some("/repo/src/app"), Some("feature/x"))];
        let out = resolve_authoritative_git(
            &events,
            |_cwd| {
                Some(GitContext {
                    remote_url: Some("git@github.com:acme/api.git".into()),
                    remote_host: Some("github.com".into()),
                    remote_slug: Some("acme/api".into()),
                    branch: Some("main".into()),
                })
            },
            |_cwd| Some("/repo".into()),
        );
        let g = out[0].git.as_ref().unwrap();
        assert_eq!(g.remote_slug.as_deref(), Some("acme/api")); // corrected
        assert_eq!(g.branch.as_deref(), Some("feature/x")); // turn branch preserved
    }

    #[test]
    fn no_remote_falls_back_to_the_repo_root_basename() {
        let events = vec![ev(Some("/home/dev/myrepo/src"), None)];
        let out = resolve_authoritative_git(
            &events,
            |_cwd| {
                Some(GitContext {
                    remote_url: None,
                    remote_host: None,
                    remote_slug: None, // no origin
                    branch: Some("dev".into()),
                })
            },
            |_cwd| Some("/home/dev/myrepo".into()),
        );
        let g = out[0].git.as_ref().unwrap();
        assert_eq!(g.remote_slug.as_deref(), Some("myrepo")); // bare root name
        assert_eq!(g.branch.as_deref(), Some("dev")); // on-disk branch (event had none)
    }

    #[test]
    fn no_git_reachable_leaves_the_event_untouched() {
        let events = vec![ev(Some("/tmp/scratch"), Some("wip"))];
        let out = resolve_authoritative_git(&events, |_| None, |_| None);
        // Unchanged: the parser's guessed slug survives.
        assert_eq!(
            out[0].git.as_ref().unwrap().remote_slug.as_deref(),
            Some("guessed/subdir")
        );
    }

    #[test]
    fn events_without_a_cwd_pass_through() {
        let events = vec![ev(None, None)];
        let out = resolve_authoritative_git(&events, |_| None, |_| None);
        assert!(out[0].git.is_none());
    }
}
