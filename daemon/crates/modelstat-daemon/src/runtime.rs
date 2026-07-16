//! Daemon-main runtime wiring — the closures + scan wrappers + the top-level
//! `run` loop that compose every M4 primitive into the live collector process.
//!
//! This file grows the daemon-main port (apps/daemon/src/daemon.ts). It starts
//! with the two remaining scan-loop closures whose construction needs concrete
//! types (`correct_events` owns a real `GitResolver`; `extract_links` is concrete
//! over `SummarizerClient` so its boxed future is `Send` — a generic engine
//! can't satisfy that under async-fn-in-trait). The boot sequence + async event
//! loop + heartbeat/last-status/shutdown land next.

use std::future::Future;
use std::pin::Pin;

use modelstat_parsers::git::resolve_repo_root;
use modelstat_parsers::GitResolver;
use modelstat_pipeline::passes::link_extract;
use modelstat_pipeline::LinkExtractor;
use modelstat_sumclient::SummarizerClient;
use modelstat_wire::RawEvent;

use crate::authoritative_git::resolve_authoritative_git;

/// The scan loop's `correct_events` seam, backed by a real cwd-cached
/// `GitResolver`: rewrites each event's repo identity to the AUTHORITATIVE on-disk
/// remote before segmentation. Owns its own resolver (kept separate from the
/// metadata git enrichment — two caches, correctness-neutral). Port of the
/// daemon's `resolveAuthoritativeGit` wiring.
pub fn make_correct_events() -> impl FnMut(Vec<RawEvent>) -> Vec<RawEvent> {
    let mut resolver = GitResolver::new();
    move |events: Vec<RawEvent>| {
        resolve_authoritative_git(
            &events,
            |cwd| resolver.resolve(Some(cwd)),
            |cwd| resolve_repo_root(Some(cwd)),
        )
    }
}

/// The session-metadata `extract_links` seam — the model call that mines
/// code-collaboration references (PR/issue URLs, `org/repo#123`, ticket keys)
/// from a session's redacted abstracts. CONCRETE over `SummarizerClient` (not a
/// generic `S: Summarizer`) so the boxed future is provably `Send`; best-effort
/// (a `None` reply just leaves the deterministic reference channels standing).
pub fn make_extract_links(engine: &SummarizerClient) -> Box<LinkExtractor<'_>> {
    Box::new(move |abstracts: Vec<String>| -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        Box::pin(async move { link_extract(engine, &abstracts).await })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(cwd: Option<&str>) -> RawEvent {
        RawEvent {
            source_event_id: "e1".into(),
            ts: "2026-07-16T10:00:00.000Z".into(),
            kind: "message".into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: None,
            session_id: "s1".into(),
            turn_index: None,
            parent_event_id: None,
            cwd: cwd.map(Into::into),
            git: None,
            tokens: None,
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: Vec::new(),
            content_excerpt: None,
            references: None,
            source_file: None,
            source_byte_offset: None,
            pricing_mode: None,
        }
    }

    #[test]
    fn correct_events_leaves_cwd_less_events_untouched() {
        // No cwd on any event → resolve_authoritative_git returns them unchanged
        // WITHOUT invoking git (deterministic; the real resolver is never called).
        let mut correct = make_correct_events();
        let events = vec![ev(None), ev(None)];
        let out = correct(events.clone());
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|e| e.git.is_none()));
    }

    // make_extract_links is a thin concrete wrapper over the (tested) link_extract
    // pass; this test just pins that it constructs + type-checks as a Send boxed
    // future (a generic engine would fail to compile here — the whole point).
    #[test]
    fn extract_links_constructs_over_a_concrete_client() {
        let client = SummarizerClient::new("http://127.0.0.1:0");
        let _extractor: Box<LinkExtractor<'_>> = make_extract_links(&client);
    }
}
