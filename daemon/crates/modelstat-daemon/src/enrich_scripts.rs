//! On-device per-script content summaries — a port of
//! `apps/daemon/src/enrich-scripts.ts::enrichToolCallScripts` + `defaultRoots`.
//!
//! A tool command often runs script/bash FILES (`./deploy.sh`, `scripts/migrate.py`).
//! `command_redacted` says the file was run, not what it DOES. This pass reads
//! each referenced file LOCALLY, compacts it to one factual sentence with the
//! bundled model, redacts that sentence, and attaches it to `ToolAction.scripts`
//! as `{ token, summary }` — where `token` is the script's exact substring of
//! `command_redacted`, so the backend zips each abstract to its place with no
//! fuzzy matching and no raw paths.
//!
//! Best-effort + additive: any failure (file missing/unreadable, model error, a
//! ref whose path redacted to a placeholder) just leaves that script out; the
//! call still ships its redacted command. The scan loop wraps this in a closure
//! and never lets it sink an upload.

use std::collections::{HashMap, HashSet};

use modelstat_parsers::{detect_script_refs, resolve_script_path, LocalToolContext, ToolCallDraft};
use modelstat_pipeline::passes::script_summary;
use modelstat_pipeline::Summarizer;
use modelstat_redact::{ner_redact, redact, NerModel};
use modelstat_wire::{ScriptSummary, ToolAction};

/// Wire cap on `ToolAction.scripts` (`z.array(...).max(8)`).
const MAX_SCRIPTS_PER_CALL: usize = 8;
/// Wire cap on `ToolAction.scripts[].summary` (`z.string().max(200)`).
const MAX_SUMMARY_CHARS: usize = 200;

/// Liberal, generic candidate roots from a cwd: the dir + a few ancestors +
/// conventional script subdirs, so a bare `deploy.sh` still resolves to
/// `<cwd>/scripts/deploy.sh`. Port of `defaultRoots`.
fn default_roots(cwd: Option<&str>) -> Vec<String> {
    let Some(cwd) = cwd else {
        return Vec::new();
    };
    let seg: Vec<&str> = cwd.trim_end_matches('/').split('/').collect();
    // Ancestors: seg[..i] for i from len down to max(1, len-3).
    let start = seg.len();
    let end = start.saturating_sub(3).max(1);
    let mut ancestors: Vec<String> = Vec::new();
    for i in (end..=start).rev() {
        let joined = seg[..i].join("/");
        ancestors.push(if joined.is_empty() {
            "/".to_string()
        } else {
            joined
        });
    }
    let subdirs = ["scripts", "bin", "tools", "ci", ".github/scripts"];
    let mut roots = ancestors.clone();
    for a in &ancestors {
        for s in subdirs {
            roots.push(format!("{a}/{s}"));
        }
    }
    roots
}

/// Enrich each draft's `ToolAction.scripts` in place from the matching local
/// context (raw command + cwd). Never panics — per-call failures are swallowed.
/// `engine` summarises a script's content; `exists`/`read_file` probe + read the
/// resolved file locally; `model_redact` (optional) is the NER pass run over the
/// model sentence AFTER the deterministic floor (defense-in-depth). Port of
/// `enrichToolCallScripts`.
pub async fn enrich_tool_call_scripts<S, N>(
    drafts: &mut [ToolCallDraft],
    contexts: &[LocalToolContext],
    engine: &S,
    // `+ Sync` so `&exists`/`&read_file` stay `Send` across this fn's awaits — it
    // runs inside the daemon's tokio-spawned scan task (Send future). Harmless to
    // the offline unit tests, which pass plain fn items (already Sync).
    exists: &(dyn Fn(&str) -> bool + Sync),
    read_file: &(dyn Fn(&str) -> Option<String> + Sync),
    model_redact: Option<&N>,
) where
    S: Summarizer,
    N: NerModel,
{
    if contexts.is_empty() {
        return;
    }
    let ctx_by_id: HashMap<&str, &LocalToolContext> = contexts
        .iter()
        .map(|c| (c.external_call_id.as_str(), c))
        .collect();
    for draft in drafts.iter_mut() {
        // Resolve the context first (immutable borrow), then take the action
        // (mutable borrow of a disjoint field).
        let ctx = match ctx_by_id.get(draft.external_call_id.as_str()) {
            Some(c) => *c,
            None => continue,
        };
        if let Some(action) = draft.action.as_mut() {
            enrich_one_action(action, ctx, engine, exists, read_file, model_redact).await;
        }
    }
}

async fn enrich_one_action<S, N>(
    action: &mut ToolAction,
    ctx: &LocalToolContext,
    engine: &S,
    exists: &(dyn Fn(&str) -> bool + Sync),
    read_file: &(dyn Fn(&str) -> Option<String> + Sync),
    model_redact: Option<&N>,
) where
    S: Summarizer,
    N: NerModel,
{
    // No redacted command → nothing on the wire to anchor a token to.
    let Some(redacted_command) = action.command_redacted.clone() else {
        return;
    };
    if redacted_command.is_empty() {
        return;
    }
    let refs = detect_script_refs(&ctx.command);
    if refs.is_empty() {
        return;
    }
    let roots = default_roots(ctx.cwd.as_deref());
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<ScriptSummary> = Vec::new();

    for reference in refs {
        if out.len() >= MAX_SCRIPTS_PER_CALL {
            break;
        }
        // The token must be EXACTLY a substring of command_redacted so the backend
        // can zip it deterministically. Redact the ref the same way the command
        // was (same cwd); a ref masked to a `[REDACTED:…]` placeholder has no
        // stable/unique token — skip it rather than ship an unlinkable abstract.
        let token = redact(&reference, ctx.cwd.as_deref())
            .text
            .trim()
            .to_string();
        if token.is_empty()
            || token.starts_with("[REDACTED")
            || seen.contains(&token)
            || !redacted_command.contains(&token)
        {
            continue;
        }
        // Resolve on disk (most-absolute / longest candidate first) + read.
        let Some(path) = resolve_script_path(&reference, &roots, exists) else {
            continue;
        };
        let Some(content) = read_file(&path) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        let Some(summary_raw) = script_summary(engine, &reference, &content).await else {
            continue;
        };
        // Defence-in-depth: the model could echo a secret/PII from the file body.
        // Regex floor first, then the optional NER pass, then cap.
        let mut summary_text = redact(&summary_raw, None).text;
        if let Some(ner) = model_redact {
            summary_text = ner_redact(ner, &summary_text).text;
        }
        let summary: String = summary_text
            .trim()
            .chars()
            .take(MAX_SUMMARY_CHARS)
            .collect();
        if summary.is_empty() {
            continue;
        }
        seen.insert(token.clone());
        out.push(ScriptSummary { token, summary });
    }

    if !out.is_empty() {
        action.scripts = out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_redact::UnavailableNer;
    use modelstat_sumclient::{CompleteRequest, SumError};

    // A fake engine: every script summarises to a fixed sentence.
    struct Fake;
    impl Summarizer for Fake {
        async fn complete(&self, _req: &CompleteRequest) -> Result<String, SumError> {
            Ok("deploys the app".into())
        }
    }

    fn action(command_redacted: &str) -> ToolAction {
        ToolAction {
            surface: "shell".into(),
            executable: Some("bash".into()),
            action: None,
            object: None,
            qualifiers: Vec::new(),
            param_shape: None,
            keywords: Vec::new(),
            r#abstract: None,
            command_redacted: Some(command_redacted.into()),
            scripts: Vec::new(),
            confidence: 0.0,
            extractor: "shell.v3".into(),
        }
    }

    fn draft(external: &str, command_redacted: &str) -> ToolCallDraft {
        ToolCallDraft {
            external_call_id: external.into(),
            session_id: "s1".into(),
            source_event_id: "e1".into(),
            agent: "claude_code".into(),
            server: "shell".into(),
            name: "bash".into(),
            turn_index: None,
            call_index: 0,
            started_at: "2026-07-16T10:00:00.000Z".into(),
            ended_at: None,
            status: "ok".into(),
            args_hash: "ah".into(),
            signature_hash: "sh".into(),
            args_bytes: 0,
            result_bytes: 0,
            model: None,
            action: Some(action(command_redacted)),
        }
    }

    fn ctx(external: &str, command: &str) -> LocalToolContext {
        LocalToolContext {
            external_call_id: external.into(),
            command: command.into(),
            cwd: Some("/repo".into()),
        }
    }

    #[tokio::test]
    async fn attaches_a_summary_for_a_resolvable_script() {
        let mut drafts = vec![draft("c1", "bash ./deploy.sh")];
        let contexts = vec![ctx("c1", "bash ./deploy.sh")];
        let exists = |_: &str| true; // any candidate "exists"
        let read = |_: &str| Some("echo deploying".to_string());
        let no_ner: Option<&UnavailableNer> = None;

        enrich_tool_call_scripts(&mut drafts, &contexts, &Fake, &exists, &read, no_ner).await;

        let scripts = &drafts[0].action.as_ref().unwrap().scripts;
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].token, "./deploy.sh");
        assert_eq!(scripts[0].summary, "deploys the app");
    }

    #[tokio::test]
    async fn a_draft_with_no_matching_context_is_left_untouched() {
        let mut drafts = vec![draft("c1", "bash ./deploy.sh")];
        let contexts = vec![ctx("OTHER", "bash ./deploy.sh")]; // different call id
        let exists = |_: &str| true;
        let read = |_: &str| Some("echo hi".to_string());
        let no_ner: Option<&UnavailableNer> = None;

        enrich_tool_call_scripts(&mut drafts, &contexts, &Fake, &exists, &read, no_ner).await;
        assert!(drafts[0].action.as_ref().unwrap().scripts.is_empty());
    }

    #[tokio::test]
    async fn an_unresolvable_script_is_skipped() {
        let mut drafts = vec![draft("c1", "bash ./deploy.sh")];
        let contexts = vec![ctx("c1", "bash ./deploy.sh")];
        let exists = |_: &str| false; // nothing resolves on disk
        let read = |_: &str| Some("echo hi".to_string());
        let no_ner: Option<&UnavailableNer> = None;

        enrich_tool_call_scripts(&mut drafts, &contexts, &Fake, &exists, &read, no_ner).await;
        assert!(drafts[0].action.as_ref().unwrap().scripts.is_empty());
    }

    #[test]
    fn default_roots_walks_ancestors_and_adds_subdirs() {
        let roots = default_roots(Some("/a/b/c"));
        // Ancestors newest-first + their script subdirs.
        assert!(roots.contains(&"/a/b/c".to_string()));
        assert!(roots.contains(&"/a/b".to_string()));
        assert!(roots.contains(&"/a/b/c/scripts".to_string()));
        assert!(roots.contains(&"/a/b/.github/scripts".to_string()));
        assert!(default_roots(None).is_empty());
    }
}
