//! Per-session metadata pass — a port of
//! `packages/daemon-core/src/pipeline/session-metadata.ts`. Assembles the repos,
//! pull requests, and issues a session touched, then ships one
//! [`SessionMetadata`] per session on the ingest batch (under
//! `session_metadata[session_id]`).
//!
//! This is the on-device half of the spend→outcome join. It fuses four channels
//! in descending order of trust (`RefSource`): (1) the git context already on
//! each event, (2) an injected best-effort read of the repo on disk
//! ([`GitEnrichment::resolve_git`]), (3) redacted content — PR/issue URLs
//! surviving in the segment abstracts + event excerpts, and (4) one best-effort
//! on-device model call per session over the redacted abstracts, whose free-text
//! reply is re-parsed deterministically. The detection + dedupe logic itself is
//! pure and lives in `modelstat_parsers::references`; this module only
//! orchestrates the channels and the (optional, best-effort) git + model I/O.
//!
//! Mirrors [`crate::build::build_session_titles`] in shape: group by session,
//! enrich, fall back gracefully — a model or git hiccup never blocks the batch.
//! It lives here (not in parsers) because it needs the titler's abstract
//! sampling/stripping; the reference + git pieces come from the parsers sibling.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;

use modelstat_parsers::{
    dedupe_files, dedupe_session_metadata, detect_branch_tickets, detect_references,
    is_empty_session_metadata, DetectedRefs, FileRef, GitEnrichment, RepoRef, SessionMetadata,
};
use modelstat_wire::{RawEvent, Segment};

use crate::passes::{sample_abstracts, strip_cognition_suffix};
use crate::prompts::LINK_EXTRACT_MAX_ABSTRACTS;

/// Async adapter for the on-device link-extraction model: takes the sampled,
/// already-redacted abstracts and returns the raw free-text reply (or `None`).
/// Mirrors the TS `LinkExtractor`; injected as a trait object so the pipeline
/// never links the engine directly — the collector builds it from the frozen
/// [`crate::prompts::LINK_EXTRACT_SYSTEM_PROMPT`] +
/// [`crate::prompts::build_link_extract_user_prompt`] + the summarizer client.
// `Send + Sync` on the outer `dyn Fn` so a `Box<LinkExtractor>` can be held
// across an await inside the daemon's single-flight scan task (which tokio spawns,
// and so requires `Send`). The one constructor — `make_extract_links` over the
// summarizer client — already satisfies both.
pub type LinkExtractor<'a> = dyn Fn(Vec<String>) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>
    + Send
    + Sync
    + 'a;

/// Commits usually land when a session wraps up — a little AFTER its active
/// window closes — so the per-file capture extends `until` by this grace period
/// (capped at the next session's start). 4h covers "commit when you're done".
const COMMIT_GRACE_MS: i64 = 4 * 60 * 60 * 1000;

/// `git.branch ? [branch] : []` — the single observed branch, or none.
fn branch_vec(branch: &Option<String>) -> Vec<String> {
    match branch {
        Some(b) if !b.is_empty() => vec![b.clone()],
        _ => Vec::new(),
    }
}

/// Insertion-ordered `Map::set`: update an existing key's value in place (keeping
/// its position, like JS `Map`), else append. Small N (a few repos per session).
fn set_slug_cwd(map: &mut Vec<(String, String)>, key: String, value: String) {
    if let Some(entry) = map.iter_mut().find(|(k, _)| *k == key) {
        entry.1 = value;
    } else {
        map.push((key, value));
    }
}

fn get_slug_cwd(map: &[(String, String)], key: &str) -> Option<String> {
    map.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

/// The session's `[since, until]` as ISO-8601 instants — the span of its
/// segments and events. `None` when nothing is timestamped. ISO-8601 sorts
/// lexically = chronologically, so a string min/max bounds the active window.
fn session_window(evs: &[&RawEvent], segs: &[&Segment]) -> Option<(String, String)> {
    let mut starts: Vec<&str> = Vec::new();
    let mut ends: Vec<&str> = Vec::new();
    for s in segs {
        if !s.started_at.is_empty() {
            starts.push(&s.started_at);
        }
        if !s.ended_at.is_empty() {
            ends.push(&s.ended_at);
        }
    }
    for e in evs {
        if !e.ts.is_empty() {
            starts.push(&e.ts);
            ends.push(&e.ts);
        }
    }
    starts.sort_unstable();
    ends.sort_unstable();
    let since = *starts.first()?;
    let until = *ends.last()?;
    Some((since.to_string(), until.to_string()))
}

/// `Date.parse` — an ISO-8601 instant to epoch-ms, or `None` when unparseable.
fn parse_iso_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// `new Date(ms).toISOString()` — epoch-ms to `YYYY-MM-DDTHH:mm:ss.sssZ`. Only
/// ever fed to `git --until`, never the wire, so exact-byte parity isn't required.
fn ms_to_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// Build one [`SessionMetadata`] per session from a batch's segments + events.
/// Sessions whose channels surface no reference are omitted (shipping an empty
/// map would only overwrite better server state). Returns a map suitable for
/// `IngestBatch.session_metadata`.
///
/// `git` is the injected best-effort git-enrichment seam (channels 2/5/6); `None`
/// disables all three git channels. `extract_links` is the injected best-effort
/// model channel (4); `None` disables it. Both failing degrades gracefully — the
/// deterministic channels stand on their own.
pub async fn build_session_metadata<'g, 'o: 'g>(
    segments: &[Segment],
    events: &[RawEvent],
    // `+ Send`: this git handle is held across the link-extract await, and the
    // whole pass runs inside the daemon's tokio-spawned scan (Send future). Every
    // real impl (RealGitEnrichment) is Send.
    //
    // `'g` (the borrow) is split from `'o` (the erased handle's own lifetime). Tied
    // together — the natural `&mut (dyn … + Send)` — a caller that reborrows one
    // long-lived handle per batch can't compile, because `&mut` is invariant in the
    // object lifetime and so won't shorten. The SDK drain does exactly that.
    mut git: Option<&'g mut (dyn GitEnrichment + Send + 'o)>,
    extract_links: Option<&LinkExtractor<'_>>,
) -> BTreeMap<String, SessionMetadata> {
    // Group by session. BTreeMap → sorted, deterministic output map; per-session
    // values are independent of iteration order (the git cache is a pure
    // cwd→context function), so a sorted walk is safe. Within a session, events
    // stay in input order (channel 1's cwd order + git-ref order depend on it).
    let mut events_by_session: BTreeMap<String, Vec<&RawEvent>> = BTreeMap::new();
    for e in events {
        events_by_session
            .entry(e.session_id.clone())
            .or_default()
            .push(e);
    }
    let mut segs_by_session: BTreeMap<String, Vec<&Segment>> = BTreeMap::new();
    for s in segments {
        segs_by_session
            .entry(s.session_id.clone())
            .or_default()
            .push(s);
    }
    let session_ids: BTreeSet<String> = events_by_session
        .keys()
        .chain(segs_by_session.keys())
        .cloned()
        .collect();

    // Every session's start instant (ms), sorted — used to cap each session's
    // commit-capture grace window (step 6) at the NEXT session's start, so a
    // later session never double-claims a commit made after this one wrapped up.
    let mut all_starts_ms: Vec<i64> = session_ids
        .iter()
        .filter_map(|sid| {
            let evs = events_by_session.get(sid).map(Vec::as_slice).unwrap_or(&[]);
            let segs = segs_by_session.get(sid).map(Vec::as_slice).unwrap_or(&[]);
            session_window(evs, segs).and_then(|(since, _)| parse_iso_ms(&since))
        })
        .collect();
    all_starts_ms.sort_unstable();

    let mut out: BTreeMap<String, SessionMetadata> = BTreeMap::new();
    for session_id in &session_ids {
        let evs = events_by_session
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let segs = segs_by_session
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut parts: Vec<DetectedRefs> = Vec::new();
        // repo slug (lowercased) → a cwd on disk for it (built in step 2), used to
        // run the verified-outcome + files git-reads against the right local repo.
        let mut slug_to_cwd: Vec<(String, String)> = Vec::new();

        // 1. git context already on the events.
        let mut cwds: Vec<String> = Vec::new();
        for e in evs {
            if let Some(c) = e.cwd.as_ref().filter(|c| !c.is_empty()) {
                if !cwds.iter().any(|x| x == c) {
                    cwds.push(c.clone());
                }
            }
            let Some(g) = &e.git else { continue };
            let mut refs = DetectedRefs::default();
            if let Some(slug) = g.remote_slug.as_ref().filter(|s| !s.is_empty()) {
                refs.repos.push(RepoRef {
                    host: g.remote_host.clone(),
                    slug: slug.clone(),
                    branches: branch_vec(&g.branch),
                    source: "git".into(),
                });
            }
            if let Some(b) = g.branch.as_ref().filter(|b| !b.is_empty()) {
                refs.issues.extend(detect_branch_tickets(Some(b)));
            }
            parts.push(refs);
        }

        // 2. resolve git on disk for the session's cwds (best-effort, cwd-cached).
        if let Some(g) = git.as_deref_mut() {
            for cwd in &cwds {
                let Some(ctx) = g.resolve_git(Some(cwd)) else {
                    continue;
                };
                let Some(slug) = ctx.remote_slug.as_ref().filter(|s| !s.is_empty()) else {
                    continue;
                };
                set_slug_cwd(&mut slug_to_cwd, slug.to_lowercase(), cwd.clone());
                let mut refs = DetectedRefs::default();
                refs.repos.push(RepoRef {
                    host: ctx.remote_host.clone(),
                    slug: slug.clone(),
                    branches: branch_vec(&ctx.branch),
                    source: "git".into(),
                });
                if let Some(b) = ctx.branch.as_ref().filter(|b| !b.is_empty()) {
                    refs.issues.extend(detect_branch_tickets(Some(b)));
                }
                parts.push(refs);
            }
        }

        // 3a. references the parser already pulled from each event's FULL text
        //     (high recall — whole turn, not just the excerpt). The opaque
        //     `RawEvent.references` blob folds back into the typed accumulator; a
        //     malformed/foreign blob is skipped (best-effort, like the TS push
        //     whose garbage the keep-valid pass would drop anyway).
        for e in evs {
            if let Some(r) = &e.references {
                if let Ok(refs) = serde_json::from_value::<DetectedRefs>(r.clone()) {
                    parts.push(refs);
                }
            }
        }
        // 3b. fallback scan of the redacted excerpts (older/replayed events).
        for e in evs {
            if let Some(ex) = e.content_excerpt.as_ref().filter(|x| !x.is_empty()) {
                parts.push(detect_references(ex, "content"));
            }
        }
        // Segment abstracts, chronological, cognition-suffix stripped, non-empty.
        let mut sorted_segs: Vec<&Segment> = segs.to_vec();
        sorted_segs.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        let abstracts: Vec<String> = sorted_segs
            .iter()
            .map(|s| strip_cognition_suffix(&s.r#abstract))
            .filter(|a| !a.is_empty())
            .collect();
        for a in &abstracts {
            parts.push(detect_references(a, "content"));
        }

        // 4. provider-agnostic model pass — one best-effort call per session over
        //    the sampled abstracts; its free-text reply is re-parsed here.
        if let Some(extract) = extract_links {
            if !abstracts.is_empty() {
                let sample = sample_abstracts(&abstracts, LINK_EXTRACT_MAX_ABSTRACTS);
                if let Some(reply) = extract(sample).await {
                    if !reply.is_empty() {
                        parts.push(detect_references(&reply, "model"));
                    }
                }
            }
        }

        let mut meta = dedupe_session_metadata(std::mem::take(&mut parts));

        // 5. enrich PRs with on-device verified-outcome signals (CPVO), where the
        //    PR's repo is on disk. Best-effort + per-PR isolated.
        if let Some(g) = git.as_deref_mut() {
            for pr in &mut meta.pull_requests {
                let cwd = pr
                    .slug
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .and_then(|s| get_slug_cwd(&slug_to_cwd, &s.to_lowercase()));
                let Some(cwd) = cwd else { continue };
                if let Some(o) = g.check_pr_outcome(&cwd, pr.number) {
                    pr.merged = Some(o.merged);
                    pr.merged_at = Some(o.merged_at);
                    pr.reverted = Some(o.reverted);
                    // The commit the `merged` reading rests on, so the server can
                    // check the convention rather than take it on faith.
                    pr.merge_sha = o.merge_sha;
                    pr.merge_subject = o.merge_subject;
                    pr.merge_method = o.merge_method.map(str::to_string);
                    // What it changed, measured off that same commit. Absent
                    // when the local repo could not say it — never zeroed, and
                    // `commits_count` is absent on its own for a squash merge.
                    if let Some(c) = o.change {
                        pr.files_changed = Some(c.files_changed);
                        pr.lines_added = Some(c.lines_added);
                        pr.lines_deleted = Some(c.lines_deleted);
                        pr.commits_count = c.commits_count;
                    }
                }
            }
        }

        // 6. enrich with the files each resolved repo changed in the session
        //    window (+ a commit-on-wrap grace, capped at the next session start).
        if let Some(g) = git.as_deref_mut() {
            if !slug_to_cwd.is_empty() {
                if let Some((since, until_raw)) = session_window(evs, segs) {
                    // `Date.parse(until)` is NaN only for a malformed timestamp;
                    // where TS would then throw in `toISOString` and drop the WHOLE
                    // session, we skip only the files step and keep the metadata
                    // found so far — no silent data loss.
                    if let Some(end_ms) = parse_iso_ms(&until_raw) {
                        let next_start = all_starts_ms.iter().copied().find(|m| *m > end_ms);
                        let until_ms = match next_start {
                            Some(ns) => (end_ms + COMMIT_GRACE_MS).min(ns),
                            None => end_ms + COMMIT_GRACE_MS,
                        };
                        let until = ms_to_iso(until_ms);
                        let mut file_refs: Vec<FileRef> = Vec::new();
                        for (slug, cwd) in &slug_to_cwd {
                            if let Some(changes) = g.collect_files_changed(cwd, &since, &until) {
                                for c in changes {
                                    file_refs.push(FileRef {
                                        slug: Some(slug.clone()),
                                        path: c.path,
                                        lines_added: c.lines_added,
                                        lines_deleted: c.lines_deleted,
                                        source: "git".into(),
                                    });
                                }
                            }
                        }
                        if !file_refs.is_empty() {
                            let mut combined = std::mem::take(&mut meta.files);
                            combined.extend(file_refs);
                            meta.files = dedupe_files(combined);
                        }
                    }
                }
            }
        }

        if !is_empty_session_metadata(&meta) {
            out.insert(session_id.clone(), meta);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_parsers::{FileChange, PrChange, PrOutcome};
    use modelstat_wire::GitContext;
    use std::collections::HashMap;

    fn ev(session: &str, ts: &str) -> RawEvent {
        RawEvent {
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            source_event_id: format!("{session}#{ts}"),
            ts: ts.into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: session.into(),
            actor_id: None,
            recipient_actor_id: None,
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: None,
            tokens_unmapped: std::collections::BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    fn seg(session: &str, started: &str, abstract_text: &str) -> Segment {
        Segment {
            segment_id: format!("{session}@{started}"),
            session_id: session.into(),
            agent: "claude_code".into(),
            started_at: started.into(),
            ended_at: started.into(),
            r#abstract: abstract_text.into(),
            tokens: Default::default(),
            tags: Vec::new(),
            redaction: Default::default(),
            source_event_ids: Vec::new(),
            abstract_embedding: None,
            behavior: None,
            user_intent: None,
            local_time: None,
        }
    }

    fn gitctx(slug: &str, host: &str, branch: &str) -> GitContext {
        GitContext {
            remote_url: None,
            remote_host: Some(host.into()),
            remote_slug: Some(slug.into()),
            branch: Some(branch.into()),
            slug_source: None,
        }
    }

    #[derive(Default)]
    struct FakeGit {
        repos: HashMap<String, GitContext>,
        outcomes: HashMap<(String, u64), PrOutcome>,
        files: HashMap<String, Vec<FileChange>>,
        files_calls: Vec<(String, String, String)>,
    }
    impl GitEnrichment for FakeGit {
        fn resolve_git(&mut self, cwd: Option<&str>) -> Option<GitContext> {
            cwd.and_then(|c| self.repos.get(c).cloned())
        }
        fn check_pr_outcome(&mut self, cwd: &str, pr_number: u64) -> Option<PrOutcome> {
            self.outcomes.get(&(cwd.to_string(), pr_number)).cloned()
        }
        fn collect_files_changed(
            &mut self,
            cwd: &str,
            since: &str,
            until: &str,
        ) -> Option<Vec<FileChange>> {
            self.files_calls
                .push((cwd.to_string(), since.to_string(), until.to_string()));
            self.files.get(cwd).cloned()
        }
    }

    #[tokio::test]
    async fn event_git_channel_yields_repo_and_branch_ticket() {
        let mut e = ev("s1", "2026-07-16T10:00:00.000Z");
        e.git = Some(gitctx("acme/api", "github.com", "feature/ENG-742-retry"));
        let out = build_session_metadata(&[], &[e], None, None).await;
        let m = out.get("s1").expect("session present");
        assert_eq!(m.repos.len(), 1);
        assert_eq!(m.repos[0].slug, "acme/api");
        assert_eq!(m.repos[0].source, "git");
        assert_eq!(
            m.repos[0].branches,
            vec!["feature/ENG-742-retry".to_string()]
        );
        // The branch's ticket key is mined as a git-sourced issue.
        assert_eq!(m.issues.len(), 1);
        assert_eq!(m.issues[0].key, "ENG-742");
        assert_eq!(m.issues[0].source, "git");
    }

    #[tokio::test]
    async fn content_channel_mines_excerpt_and_abstract() {
        // No git injected — the deterministic content channels stand alone.
        let mut e = ev("s1", "2026-07-16T10:00:00.000Z");
        e.content_excerpt = Some("opened https://github.com/acme/api/pull/42".into());
        let s = seg(
            "s1",
            "2026-07-16T10:00:00.000Z",
            "Fixed acme/web#7 in the auth layer",
        );
        let out = build_session_metadata(&[s], &[e], None, None).await;
        let m = out.get("s1").expect("session present");
        assert!(m
            .pull_requests
            .iter()
            .any(|p| p.number == 42 && p.slug.as_deref() == Some("acme/api")));
        assert!(m.issues.iter().any(|i| i.key == "7"));
    }

    #[tokio::test]
    async fn resolve_git_populates_repo_and_pr_outcome() {
        let mut e = ev("s1", "2026-07-16T10:00:00.000Z");
        e.cwd = Some("/home/dev/api".into());
        let s = seg(
            "s1",
            "2026-07-16T10:00:00.000Z",
            "Merged https://github.com/acme/api/pull/42",
        );
        let mut fake = FakeGit::default();
        fake.repos.insert(
            "/home/dev/api".into(),
            gitctx("acme/api", "github.com", "main"),
        );
        fake.outcomes.insert(
            ("/home/dev/api".into(), 42),
            PrOutcome {
                merged: true,
                merged_at: Some("2026-07-16T11:00:00Z".into()),
                reverted: false,
                merge_sha: Some("c0ffee1".into()),
                merge_subject: Some("feat: retries (#42)".into()),
                merge_method: Some("subject_ref_convention"),
                change: Some(PrChange {
                    files_changed: 3,
                    lines_added: 120,
                    lines_deleted: 4,
                    // A squash merge: the branch is gone, the diff is not.
                    commits_count: None,
                }),
            },
        );
        let out = build_session_metadata(&[s], &[e], Some(&mut fake), None).await;
        let m = out.get("s1").expect("session present");
        // The repo is resolved authoritatively from disk (git source wins dedupe).
        assert!(m
            .repos
            .iter()
            .any(|r| r.slug == "acme/api" && r.source == "git"));
        let pr = m
            .pull_requests
            .iter()
            .find(|p| p.number == 42)
            .expect("pr present");
        assert_eq!(pr.merged, Some(true));
        assert_eq!(pr.merged_at, Some(Some("2026-07-16T11:00:00Z".into())));
        assert_eq!(pr.reverted, Some(false));
        // The enrichment carries its evidence all the way onto the wire ref.
        assert_eq!(pr.merge_sha.as_deref(), Some("c0ffee1"));
        assert_eq!(pr.merge_subject.as_deref(), Some("feat: retries (#42)"));
        assert_eq!(pr.merge_method.as_deref(), Some("subject_ref_convention"));
        // …and so do the change primitives it measured off that commit.
        assert_eq!(pr.files_changed, Some(3));
        assert_eq!(pr.lines_added, Some(120));
        assert_eq!(pr.lines_deleted, Some(4));
        assert_eq!(
            pr.commits_count, None,
            "a squash merge's branch count is unknown, and stays unknown"
        );
    }

    #[tokio::test]
    async fn files_channel_attaches_deduped_slug_stamped_refs() {
        let mut e = ev("s1", "2026-07-16T10:00:00.000Z");
        e.cwd = Some("/home/dev/api".into());
        let mut fake = FakeGit::default();
        fake.repos.insert(
            "/home/dev/api".into(),
            gitctx("acme/api", "github.com", "main"),
        );
        fake.files.insert(
            "/home/dev/api".into(),
            vec![
                FileChange {
                    path: "src/a.ts".into(),
                    lines_added: 3,
                    lines_deleted: 1,
                },
                FileChange {
                    path: "src/a.ts".into(),
                    lines_added: 2,
                    lines_deleted: 0,
                },
            ],
        );
        let out = build_session_metadata(&[], &[e], Some(&mut fake), None).await;
        let m = out.get("s1").expect("session present");
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].path, "src/a.ts");
        assert_eq!(m.files[0].slug.as_deref(), Some("acme/api"));
        assert_eq!(m.files[0].lines_added, 5);
        assert_eq!(m.files[0].lines_deleted, 1);
    }

    #[tokio::test]
    async fn model_channel_reparses_the_reply() {
        let s = seg("s1", "2026-07-16T10:00:00.000Z", "Did some work");
        let extractor = |_: Vec<String>| {
            Box::pin(async { Some("https://github.com/acme/api/pull/99".to_string()) })
                as Pin<Box<dyn Future<Output = Option<String>> + Send>>
        };
        let out = build_session_metadata(&[s], &[], None, Some(&extractor as &LinkExtractor)).await;
        let m = out.get("s1").expect("session present");
        let pr = m
            .pull_requests
            .iter()
            .find(|p| p.number == 99)
            .expect("pr present");
        assert_eq!(pr.source, "model");
    }

    #[tokio::test]
    async fn session_without_refs_is_omitted() {
        let s = seg(
            "s1",
            "2026-07-16T10:00:00.000Z",
            "Refactored code, no links",
        );
        let e = ev("s1", "2026-07-16T10:00:00.000Z");
        let out = build_session_metadata(&[s], &[e], None, None).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn commit_grace_window_is_capped_at_next_session_start() {
        // s1 ends 10:00; s2 starts 11:00 (< the 4h grace). s1's file capture
        // `until` must be the next session's start, not end+4h.
        let mut e1 = ev("s1", "2026-07-16T10:00:00.000Z");
        e1.cwd = Some("/repo".into());
        let e2 = ev("s2", "2026-07-16T11:00:00.000Z");
        let mut fake = FakeGit::default();
        fake.repos
            .insert("/repo".into(), gitctx("acme/api", "github.com", "main"));
        fake.files.insert(
            "/repo".into(),
            vec![FileChange {
                path: "a".into(),
                lines_added: 1,
                lines_deleted: 0,
            }],
        );
        let _ = build_session_metadata(&[], &[e1, e2], Some(&mut fake), None).await;
        let call = fake
            .files_calls
            .iter()
            .find(|(cwd, _, _)| cwd == "/repo")
            .expect("files reads for /repo");
        assert_eq!(call.1, "2026-07-16T10:00:00.000Z"); // since = s1 window start
        assert_eq!(call.2, "2026-07-16T11:00:00.000Z"); // until capped at s2 start
    }
}
