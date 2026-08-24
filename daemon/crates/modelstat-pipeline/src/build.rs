//! Session→segment assembly (feature §18, plan §5 M4) — the port of
//! `buildForOneSession` + `summariseSlice` from
//! `packages/daemon-core/src/pipeline/index.ts`, under the rewrite's FAIL-LOUD
//! contract:
//!
//! - The **summarize** pass is REQUIRED and goes through the resilient
//!   hold-and-retry wrapper (§9.4). A down/loading/erroring engine makes it
//!   return [`SummarizeOutcome::Held`], and this builder then holds the WHOLE
//!   session ([`BuildOutcome::Held`]) — never a partial batch, because advancing
//!   the cursor past un-summarised events would drop them (§21.2 never-drop).
//! - A slice with **0 content excerpts** (a pure-tool_use slice) is a per-slice
//!   error — logged loudly and **skipped**, the batch continues (§18 line 1130).
//!   This is NOT a hold: it's a permanent property of the slice, not the engine.
//! - Every other pass (cognition, user-intent, tags, embedding) is **best-effort**
//!   — a failure is a missing signal, never a hold. Those call the raw engine via
//!   [`ResilientSummarizer::engine`], never the hold-and-retry path.
//!
//! Abstract redaction is layer 1 (the compiled-in [`redact`] floor, ALWAYS) +
//! layer 2 (the injected [`PiiModel`] seam, best-effort — the daemon wires the
//! candle BERT-PII; tests pass a fake that answers). Layer 3 (the LLM backstop) is
//! command-only (§9.5), never applied to abstracts.

use std::collections::{BTreeMap, HashSet};

use modelstat_redact::{pii_redact_checked, redact, PiiModel};
use modelstat_sumclient::CompleteRequest;
use modelstat_wire::{
    segment_id, slug_is_verified, RawEvent, RedactionReport, Segment, SegmentBehavior,
    SegmentLocalTime, TaxonomyHintRooted, TokenUsage, PROJECT_SLUG_CONFIDENCE_GUESS,
    PROJECT_SLUG_CONFIDENCE_VERIFIED,
};

use crate::embed::{Embedder, EMBED_DIM};
use crate::passes::{
    build_title_user_prompt, cognition, cognition_hints, fallback_title, format_cognition_suffix,
    sample_abstracts, sanitise_title, strip_cognition_suffix, summarize, title, CognitionTags,
    THINKING_HEADROOM_TOKENS,
};
use crate::prompts::{
    build_summariser_user_prompt, build_user_intent_user_prompt, ABSTRACT_MAX_CHARS,
    ABSTRACT_OUTPUT_MAX_CHARS, EXCERPT_SAMPLE_MAX_CHARS, SUMMARISER_SYSTEM_PROMPT,
    SUMMARISER_TEMPERATURE, SUMMARISER_TOP_K, TITLER_MAX_ABSTRACTS, USER_INTENT_MAX_CHARS,
    USER_INTENT_MAX_TOKENS, USER_INTENT_SAMPLE_HEAD, USER_INTENT_SAMPLE_TAIL,
};
use crate::resilient::{ResilientSummarizer, SummarizeOutcome, Summarizer};
use crate::segment::{parse_ts_ms, segment_turns, turn_meta, turn_surface};

/// The result of building one session's segments.
#[derive(Debug, Clone, PartialEq)]
pub enum BuildOutcome {
    /// Segments built. Some slices may have been skipped loudly (0 content
    /// excerpts, §18) but the engine was healthy throughout, so the batch is
    /// complete and its cursor may advance.
    Ready(Vec<Segment>),
    /// The summarizer engine is unavailable — the WHOLE session is held (never a
    /// partial batch). The scan loop leaves the cursor un-advanced and retries
    /// next cycle, producing the real abstracts once the engine recovers (§9.4).
    Held,
}

/// Build every [`Segment`] for ONE session's events (all sharing a `session_id`).
///
/// Sorts by `ts`, embeds each turn's redaction-safe metadata surface, detects
/// segment boundaries ([`segment_turns`] — with the singleton merge), then
/// summarises + tags each slice. Returns [`BuildOutcome::Held`] the moment the
/// required summarize pass is held (engine down); otherwise [`BuildOutcome::Ready`].
pub async fn build_for_one_session<S, E, N>(
    events: &[RawEvent],
    resilient: &ResilientSummarizer<S>,
    embedder: &E,
    redactor: &N,
) -> BuildOutcome
where
    S: Summarizer,
    E: Embedder,
    N: PiiModel,
{
    if events.is_empty() {
        return BuildOutcome::Ready(Vec::new());
    }
    // Sort by ts. JS `localeCompare` on ISO-8601 == lexicographic == `str::cmp`.
    let mut sorted: Vec<&RawEvent> = events.iter().collect();
    sorted.sort_by(|a, b| a.ts.cmp(&b.ts));
    let session_id = sorted[0].session_id.as_str();

    // Per-turn metadata-only embeddings (redaction-safe surface). A failure just
    // leaves that turn's vector empty and the cosine check skips its pairs — the
    // scan never dies because a model is missing (§9.5).
    let turns: Vec<_> = sorted
        .iter()
        .map(|&e| {
            let surface = turn_surface(e);
            let embedding = if surface.is_empty() {
                Vec::new()
            } else {
                embedder.embed(&surface)
            };
            turn_meta(e, embedding)
        })
        .collect();

    let groups = segment_turns(&turns); // boundary detection + singleton merge

    let mut segments = Vec::with_capacity(groups.len());
    for group in groups {
        let slice: Vec<&RawEvent> = group.iter().map(|&i| sorted[i]).collect();
        match summarise_slice(session_id, &slice, resilient, embedder, redactor).await {
            SliceOutcome::Built(seg) => segments.push(*seg),
            SliceOutcome::Skipped => {}
            SliceOutcome::Held => return BuildOutcome::Held,
        }
    }
    BuildOutcome::Ready(segments)
}

/// Build one dashboard title per session from a batch's segments (title.ts
/// `buildSessionTitles`). Groups by session (chronological within each), asks the
/// titler once per session, sanitises, and falls back to a deterministic
/// first-sentence title when the model is unavailable or returns noise. Sessions
/// whose segments carry no usable abstract are omitted — shipping an empty title
/// would only overwrite better server state.
///
/// Best-effort throughout: the titler is the raw engine (a failure is a fallback,
/// never a hold). Returns a map suitable for `IngestBatch.session_titles`.
pub async fn build_session_titles<S: Summarizer>(
    segments: &[Segment],
    engine: &S,
) -> BTreeMap<String, String> {
    // Group by session. The result is session-keyed, so grouping order is
    // immaterial; BTreeMap keeps iteration deterministic for tests.
    let mut by_session: BTreeMap<&str, Vec<&Segment>> = BTreeMap::new();
    for s in segments {
        by_session.entry(s.session_id.as_str()).or_default().push(s);
    }

    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (session_id, mut segs) in by_session {
        segs.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        let abstracts: Vec<String> = segs
            .iter()
            .map(|s| strip_cognition_suffix(&s.r#abstract))
            .filter(|a| !a.is_empty())
            .collect();
        if abstracts.is_empty() {
            continue;
        }
        // Ground the titler with cheap facts: the project (if tagged) + part count.
        let first = segs[0];
        let mut facts_parts: Vec<String> = Vec::new();
        if let Some(project) = first
            .tags
            .iter()
            .find(|t| t.root_key == "projects")
            .map(|t| t.name.as_str())
        {
            facts_parts.push(format!("repo {project}"));
        }
        facts_parts.push(format!(
            "{} part{} on {}",
            segs.len(),
            if segs.len() == 1 { "" } else { "s" },
            first.agent
        ));
        let facts = facts_parts.join("; ");

        let prompt = build_title_user_prompt(
            &sample_abstracts(&abstracts, TITLER_MAX_ABSTRACTS),
            Some(&facts),
        );
        let mut session_title = title(engine, &prompt)
            .await
            .map(|raw| sanitise_title(&raw))
            .unwrap_or_default();
        if session_title.is_empty() {
            session_title = fallback_title(&abstracts);
        }
        if !session_title.is_empty() {
            out.insert(session_id.to_string(), session_title);
        }
    }
    out
}

/// The outcome of summarising one slice. `Built` on success, `Skipped` for a
/// 0-excerpt slice (per-slice error, batch continues), `Held` when the engine is
/// down (the caller holds the whole session).
enum SliceOutcome {
    Built(Box<Segment>),
    Skipped,
    Held,
}

async fn summarise_slice<S, E, N>(
    session_id: &str,
    slice: &[&RawEvent],
    resilient: &ResilientSummarizer<S>,
    embedder: &E,
    redactor: &N,
) -> SliceOutcome
where
    S: Summarizer,
    E: Embedder,
    N: PiiModel,
{
    if slice.is_empty() {
        return SliceOutcome::Skipped;
    }
    let first = slice[0];
    let last = slice[slice.len() - 1];
    let started_at_ms = parse_ts_ms(&first.ts);
    let ended_at_ms = parse_ts_ms(&last.ts);

    // Token totals from the upstream parser (exact for JSONL).
    let mut tokens = TokenUsage::default();
    for ev in slice {
        if let Some(t) = &ev.tokens {
            tokens.input += t.input;
            tokens.output += t.output;
            tokens.cache_creation += t.cache_creation;
            tokens.cache_read += t.cache_read;
            tokens.reasoning += t.reasoning;
        }
    }

    // Sampled + re-redacted excerpts. 0 excerpts ⇒ per-slice error, slice skipped
    // loudly (§18): summarising metadata-only would yield "N turns on <agent>",
    // which downstream rejects. Fail at the source so the logs name the cause.
    let excerpts = sample_and_redact_excerpts(slice);
    if excerpts.is_empty() {
        modelstat_log::log_warn!(
            "slice skipped in session {session_id} ({} turns) — parser produced 0 content excerpts (would summarise metadata-only); check the {} parser",
            slice.len(),
            first.agent
        );
        return SliceOutcome::Skipped;
    }

    // Summarize — REQUIRED, resilient. Held (engine down) ⇒ hold the whole session.
    let facts = build_prompt_facts(first, slice.len());
    let user_prompt = build_summariser_user_prompt(&facts, &excerpts);
    let raw_abstract = match summarize(resilient, &user_prompt).await {
        SummarizeOutcome::Done(text) => text,
        SummarizeOutcome::Held => return SliceOutcome::Held,
    };

    // Redact the abstract: layer-1 floor (always) + layer-2 PII, FAIL-CLOSED.
    //
    // The abstract is uploaded in every mode — that is the whole point of local
    // mode — so "the raw turns never leave this machine" is only true if what the
    // summariser read was scrubbed. A turn the redactor could not classify has not
    // been scrubbed, so this holds the session rather than shipping a summary of
    // text nobody checked. Same rule the cloud path already follows; local and
    // self-hosted egress are no less egress.
    let floor = redact(&raw_abstract, None);
    let Some(model_pass) = pii_redact_checked(redactor, &floor.text) else {
        modelstat_log::log_warn!(
            "redactor could not classify a {}-char abstract — holding this session \
             rather than uploading an unscrubbed summary",
            floor.text.len()
        );
        return SliceOutcome::Held;
    };
    let redacted_text = model_pass.text;
    let redaction = RedactionReport {
        secrets_found: floor.counts.secrets_found,
        emails_redacted: floor.counts.emails_redacted,
        paths_redacted_absolute: floor.counts.paths_redacted_absolute,
        extra: model_pass.counts, // pf_<type> keys
    };

    // Cognition (best-effort) → its `[Mood: …] [Mind: …] [Stance: …]` suffix rides
    // inside the abstract text (no wire field). None on failure/trivial abstract.
    let cog: Option<CognitionTags> = cognition(resilient.engine(), &redacted_text).await;
    let suffix = cog
        .as_ref()
        .map(format_cognition_suffix)
        .unwrap_or_default();
    let abstract_with_cognition = if suffix.is_empty() {
        redacted_text.clone()
    } else {
        format!("{redacted_text} {suffix}")
    };

    // Privacy-preserving behavioral counts — never raw text, never a score.
    let behavior = compute_behavior(slice);

    // Deterministic tags from event metadata, in the frozen order (§18).
    let mut tags: Vec<TaxonomyHintRooted> = Vec::new();
    tags.push(hint("agents", &first.agent, 1.0));
    tags.push(hint("providers", &first.provider, 1.0));
    if let Some(model) = &first.model {
        tags.push(hint("models", model, 1.0));
    }
    // The projects hint reads the first event whose slug is VERIFIED
    // (`slug_is_verified`), falling back to the first event with any slug — a
    // slice can open on a guessed context (cwd outside the repo) and reach the
    // real one a turn later. Confidence states the provenance tier so the
    // server can gate project-node minting on verified identity; `reason`
    // carries the event's `slug_source` verbatim so the server reads the exact
    // tier.
    let project_git = slice
        .iter()
        .filter_map(|e| e.git.as_ref())
        .find(|g| g.remote_slug.is_some() && slug_is_verified(g))
        .or_else(|| {
            slice
                .iter()
                .filter_map(|e| e.git.as_ref())
                .find(|g| g.remote_slug.is_some())
        });
    if let Some(git) = project_git {
        if let Some(slug) = &git.remote_slug {
            let confidence = if slug_is_verified(git) {
                PROJECT_SLUG_CONFIDENCE_VERIFIED
            } else {
                PROJECT_SLUG_CONFIDENCE_GUESS
            };
            let mut h = hint("projects", slug, confidence);
            h.reason = git.slug_source.clone();
            tags.push(h);
        }
        // No `environments` hint: the branch ships verbatim in GitContext and
        // the server owns what a branch name means for a given org.
    }
    for c in components_from_slice(slice) {
        tags.push(hint("components", &c, 0.6));
    }
    // WHEN the work happened, in the engineer's own wall clock — the daemon is
    // the only place that knows it, and the server only ever sees UTC (§18).
    // The reading rides on the segment; the buckets are derived from it.
    let local_time = local_time_of(started_at_ms);
    if let Some(local) = local_time {
        tags.extend(temporal_hints(local));
    }
    // Mood/Mind/Posture primaries from the best-effort cognition pass.
    if let Some(c) = &cog {
        tags.extend(cognition_hints(c));
    }
    // Tool-call mix — top-8 identities by share-of-calls.
    tags.extend(tool_call_tags(slice));

    // Segment embedding = the redacted abstract (pre-suffix), sliced to the
    // storage cap, embedded; kept only when the backend produced a full vector.
    let embed_input = take_chars(&redacted_text, ABSTRACT_MAX_CHARS);
    let embedded = embedder.embed(&embed_input);
    let abstract_embedding: Option<Vec<f64>> = if embedded.len() == EMBED_DIM {
        Some(embedded.iter().map(|&x| x as f64).collect())
    } else {
        None
    };

    // User-intent distillation — from the developer's OWN messages, best-effort.
    let user_intent = summarise_user_intent(slice, resilient.engine(), redactor).await;

    let source_event_ids: Vec<String> = slice.iter().map(|e| e.source_event_id.clone()).collect();
    let id = segment_id(session_id, started_at_ms, ended_at_ms, &source_event_ids);
    let seg = Segment {
        segment_id: id,
        session_id: session_id.to_string(),
        agent: first.agent.clone(),
        started_at: first.ts.clone(),
        ended_at: last.ts.clone(),
        // Slice to the user-visible cap; the wire byte-clamp at send is the final
        // guarantee. Models occasionally overshoot the prompt's "≤N chars".
        r#abstract: take_chars(&abstract_with_cognition, ABSTRACT_OUTPUT_MAX_CHARS),
        tokens,
        tags,
        redaction,
        source_event_ids,
        abstract_embedding,
        behavior: Some(behavior),
        user_intent,
        local_time,
    };
    SliceOutcome::Built(Box::new(seg))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn hint(root_key: &str, name: &str, confidence: f64) -> TaxonomyHintRooted {
    TaxonomyHintRooted {
        root_key: root_key.to_string(),
        name: name.to_string(),
        confidence,
        reason: None,
    }
}

/// The summariser facts line: `repo …; branch …; N turns on <agent>; files
/// touched: …; tool calls: …` (index.ts `summariseSlice`). "N turns on agent" is
/// always present; the rest are conditional.
fn build_prompt_facts(first: &RawEvent, slice_len: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(git) = &first.git {
        if let Some(slug) = &git.remote_slug {
            parts.push(format!("repo {slug}"));
        }
        if let Some(branch) = &git.branch {
            parts.push(format!("branch {branch}"));
        }
    }
    parts.push(format!("{slice_len} turns on {}", first.agent));
    if !first.files_touched.is_empty() {
        let files: Vec<&str> = first
            .files_touched
            .iter()
            .take(5)
            .map(String::as_str)
            .collect();
        parts.push(format!("files touched: {}", files.join(", ")));
    }
    if !first.tool_calls.is_empty() {
        // BTreeMap keys are sorted (TS used object insertion order) — a benign
        // divergence in the PROMPT only, absorbed by PROCESSING_VERSION 16.
        let tools: Vec<&str> = first
            .tool_calls
            .keys()
            .take(5)
            .map(String::as_str)
            .collect();
        parts.push(format!("tool calls: {}", tools.join(", ")));
    }
    parts.join("; ")
}

/// Pick representative excerpts (first, last, quartiles ≤5) and re-run the
/// redaction floor over each as defence-in-depth, capping to 200 chars. Skips
/// events without content; returns empty when the slice has no prose at all
/// (index.ts `sampleAndRedactExcerpts`).
fn sample_and_redact_excerpts(slice: &[&RawEvent]) -> Vec<String> {
    let with_content: Vec<&str> = slice
        .iter()
        .filter_map(|e| e.content_excerpt.as_deref())
        .filter(|c| !c.trim().is_empty())
        .collect();
    if with_content.is_empty() {
        return Vec::new();
    }
    let mut picks: Vec<usize> = vec![0]; // first
    if with_content.len() > 1 {
        picks.push(with_content.len() - 1); // last
    }
    for frac in [0.25f64, 0.5, 0.75] {
        let idx = (with_content.len() as f64 * frac).floor() as usize;
        if !picks.contains(&idx) {
            picks.push(idx);
        }
        if picks.len() >= 5 {
            break;
        }
    }
    picks.sort_unstable();
    picks
        .iter()
        .map(|&i| {
            let redacted = redact(with_content[i], None).text;
            take_chars(&redacted, EXCERPT_SAMPLE_MAX_CHARS)
        })
        .collect()
}

/// Distill what the DEVELOPER asked for from their OWN messages only — the source
/// Insights' rule + skill detectors mine (index.ts `summariseUserIntent`).
/// Best-effort: uses the raw engine (a failure is `None`, never a session hold),
/// then floor + PII redacts, trims, caps to 240 chars.
async fn summarise_user_intent<S, N>(
    slice: &[&RawEvent],
    engine: &S,
    redactor: &N,
) -> Option<String>
where
    S: Summarizer,
    N: PiiModel,
{
    let user_excerpts: Vec<String> = slice
        .iter()
        .filter(|e| e.kind == "user_message")
        .filter_map(|e| e.content_excerpt.as_deref())
        .map(collapse_ws)
        .filter(|s| !s.is_empty())
        .collect();
    if user_excerpts.is_empty() {
        return None;
    }
    // The ask is usually first; later messages add direction/corrections.
    let sample: Vec<String> =
        if user_excerpts.len() <= USER_INTENT_SAMPLE_HEAD + USER_INTENT_SAMPLE_TAIL {
            user_excerpts
        } else {
            let head = &user_excerpts[..USER_INTENT_SAMPLE_HEAD];
            let tail = &user_excerpts[user_excerpts.len() - USER_INTENT_SAMPLE_TAIL..];
            head.iter().chain(tail).cloned().collect()
        };
    let req = CompleteRequest {
        system: SUMMARISER_SYSTEM_PROMPT.to_string(),
        user: build_user_intent_user_prompt(&sample),
        temperature: SUMMARISER_TEMPERATURE,
        max_tokens: USER_INTENT_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
        top_k: Some(SUMMARISER_TOP_K),
    };
    let raw = match engine.complete(&req).await {
        Ok(t) if !t.trim().is_empty() => t,
        _ => return None,
    };
    let floored = redact(&raw, None).text;
    // Fail-closed like everywhere else: no scrub, no user_intent. Dropping the
    // field costs one nicety; shipping an unscrubbed one costs a person's name.
    let scrubbed = pii_redact_checked(redactor, &floored)?.text;
    let trimmed = take_chars(scrubbed.trim(), USER_INTENT_MAX_CHARS);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Per-segment behavioral COUNTS — never raw text, and never a score.
/// `correction_count` = user messages that land right after an assistant message.
///
/// There is no `frustration` any more. It was `max(correction_count / 4, 0.8 if
/// any emotion tag contains one of nine English stems)`: two hard-coded numbers
/// and a substring list scoring an LLM's own free-text output, on a device that
/// cannot revise either. Both inputs already ship — the counts here, the emotion
/// tags in the abstract's `[Mood: …]` suffix and the `mood` hint — so the score
/// is the server's to compute, where it can be changed without a fleet release.
fn compute_behavior(slice: &[&RawEvent]) -> SegmentBehavior {
    let mut user_turns = 0u64;
    let mut correction_count = 0u64;
    let mut prev_was_assistant = false;
    for ev in slice {
        match ev.kind.as_str() {
            "user_message" => {
                user_turns += 1;
                if prev_was_assistant {
                    correction_count += 1;
                }
                prev_was_assistant = false;
            }
            "assistant_message" => prev_was_assistant = true,
            _ => {}
        }
    }
    SegmentBehavior {
        user_turns,
        correction_count,
        frustration: None,
    }
}

/// The daemon machine's local wall clock at a slice's start — the READING, not a
/// bucket. Only the device can know this (everything on the wire is UTC), so it
/// is the one thing here that is strictly lost if it is not shipped.
fn local_time_of(started_at_ms: i64) -> Option<SegmentLocalTime> {
    use chrono::{Datelike, Local, Offset, TimeZone, Timelike};
    let dt = Local.timestamp_millis_opt(started_at_ms).single()?;
    Some(SegmentLocalTime {
        // The offset in force at THAT instant, so a DST boundary reads
        // correctly rather than through today's rule.
        utc_offset_minutes: dt.offset().fix().local_minus_utc() / 60,
        hour: dt.hour() as u8,
        weekday: dt.weekday().num_days_from_sunday() as u8, // 0=Sun..6=Sat
    })
}

/// Local-wall-clock taxonomy hints for a slice's start (index.ts `temporalHints`).
///
/// These are a CUT of [`local_time_of`] — where Morning ends, whether Friday is
/// its own thing — made on a device that cannot revise it. They ride one more
/// release beside the reading they are derived from; the server owns the cut
/// once it reads `local_time`.
fn temporal_hints(local: SegmentLocalTime) -> Vec<TaxonomyHintRooted> {
    vec![
        hint("time_of_day", time_of_day_bucket(local.hour), 1.0),
        hint("cadence", cadence_bucket(local.weekday), 1.0),
    ]
}

/// Morning 5–12 / Midday 12–17 / Evening 17–21 / Night otherwise (§18).
fn time_of_day_bucket(hour: u8) -> &'static str {
    if (5..12).contains(&hour) {
        "Morning"
    } else if (12..17).contains(&hour) {
        "Midday"
    } else if (17..21).contains(&hour) {
        "Evening"
    } else {
        "Night"
    }
}

/// Weekend (Sat/Sun) / Friday / Weekday (§18). `day` is JS `getDay()`: 0=Sun..6=Sat.
fn cadence_bucket(day: u8) -> &'static str {
    if day == 0 || day == 6 {
        "Weekend"
    } else if day == 5 {
        "Friday"
    } else {
        "Weekday"
    }
}

/// Unique top-2-level dirs of every event's `files_touched`, first-seen order, ≤8.
fn components_from_slice(slice: &[&RawEvent]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut ordered: Vec<String> = Vec::new();
    for ev in slice {
        for f in &ev.files_touched {
            let seg: String = f.split('/').take(2).collect::<Vec<_>>().join("/");
            if seg.is_empty() {
                continue;
            }
            if seen.insert(seg.clone()) {
                ordered.push(seg);
            }
        }
    }
    ordered.truncate(8);
    ordered
}

/// Aggregate the slice's per-event tool-call counts, keep the top-8 identities
/// (≤120 UTF-16 units), and tag each with its share-of-calls (floored at 0.05).
fn tool_call_tags(slice: &[&RawEvent]) -> Vec<TaxonomyHintRooted> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut total: u64 = 0;
    for ev in slice {
        for (identity, &n) in &ev.tool_calls {
            if n == 0 {
                continue;
            }
            *counts.entry(identity.clone()).or_insert(0) += n;
            total += n;
        }
    }
    if total == 0 {
        return Vec::new();
    }
    let mut entries: Vec<(String, u64)> = counts
        .into_iter()
        .filter(|(identity, _)| js_len(identity) <= 120)
        .collect();
    // count desc, then identity asc (JS `localeCompare` ~ `str::cmp` for ASCII).
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries.truncate(8);
    entries
        .into_iter()
        .map(|(identity, count)| {
            let share = ((count as f64 / total as f64) * 100.0).round() / 100.0;
            let confidence = share.clamp(0.05, 1.0);
            hint("tool_calls", &identity, confidence)
        })
        .collect()
}

/// JS `String.length` (UTF-16 code units) — the cap unit tool identities use.
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// First `n` Unicode scalar values (JS `.slice(0, n)` is UTF-16; the abstracts /
/// excerpts here are BMP-dominated and the wire byte-clamp is the final guard).
fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Collapse whitespace runs to one space and trim (JS `.replace(/\s+/g," ").trim()`).
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::NoEmbedder;
    use modelstat_redact::{PiiToken, UnavailableRedactor};

    /// A redactor that ANSWERS and finds nothing. Needed now that every egress
    /// path — including the uploaded abstract — holds when the redactor cannot
    /// read a turn. `UnavailableRedactor` still means "no redactor", and the hold test
    /// below uses it for exactly that.
    struct AnsweringRedactor;
    impl PiiModel for AnsweringRedactor {
        fn classify(&self, _text: &str) -> Option<Vec<PiiToken>> {
            Some(Vec::new())
        }
    }
    use modelstat_sumclient::SumError;
    use modelstat_wire::GitContext;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A fake engine: returns a fixed reply, or fails every call.
    struct Fake {
        reply: String,
        fail: bool,
        calls: AtomicUsize,
    }
    impl Fake {
        fn reply(r: &str) -> Self {
            Self {
                reply: r.into(),
                fail: false,
                calls: AtomicUsize::new(0),
            }
        }
        fn failing() -> Self {
            Self {
                reply: String::new(),
                fail: true,
                calls: AtomicUsize::new(0),
            }
        }
    }
    impl Summarizer for Fake {
        async fn complete(&self, _req: &CompleteRequest) -> Result<String, SumError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(SumError::Http(503))
            } else {
                Ok(self.reply.clone())
            }
        }
    }

    /// A fake embedder that always returns a full 384-dim vector.
    struct FixedEmbedder;
    impl Embedder for FixedEmbedder {
        fn embed(&self, _text: &str) -> Vec<f32> {
            vec![0.1; EMBED_DIM]
        }
    }

    fn resilient(f: Fake) -> ResilientSummarizer<Fake> {
        ResilientSummarizer::with_cooldown(f, Duration::ZERO)
    }

    fn ev(id: &str, ts: &str, kind: &str, content: Option<&str>) -> RawEvent {
        RawEvent {
            seq: None,
            started_at: None,
            first_token_at: None,
            content_bytes: None,
            reasoning_excerpt: None,
            reasoning_bytes: None,
            source_event_id: id.into(),
            ts: ts.into(),
            kind: kind.into(),
            agent: "claude_code".into(),
            provider: "anthropic".into(),
            model: Some("claude-opus-4-8".into()),
            session_id: "s1".into(),
            actor_id: None,
            recipient_actor_id: None,
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
            tokens: Some(TokenUsage {
                input: 10,
                output: 5,
                cache_creation: 0,
                cache_read: 0,
                reasoning: 0,
            }),
            tokens_unmapped: std::collections::BTreeMap::new(),
            duration_ms: None,
            tool_calls: Default::default(),
            files_touched: vec![],
            tool_paths: Vec::new(),
            content_excerpt: content.map(str::to_string),
            references: None,
            source_file: None,
            source_byte_offset: None,
            redactions: Default::default(),
        }
    }

    #[tokio::test]
    async fn builds_one_segment_from_a_healthy_session() {
        let events = vec![
            ev(
                "e1",
                "2026-06-01T10:00:00.000Z",
                "user_message",
                Some("fix the auth bug"),
            ),
            ev(
                "e2",
                "2026-06-01T10:00:30.000Z",
                "assistant_message",
                Some("done, patched middleware"),
            ),
        ];
        let r = resilient(Fake::reply("Fixed a null deref in the auth middleware"));
        match build_for_one_session(&events, &r, &NoEmbedder, &AnsweringRedactor).await {
            BuildOutcome::Ready(segs) => {
                assert_eq!(segs.len(), 1);
                let s = &segs[0];
                assert_eq!(s.session_id, "s1");
                assert_eq!(s.r#abstract, "Fixed a null deref in the auth middleware");
                assert_eq!(s.source_event_ids, vec!["e1", "e2"]);
                // tokens summed across the slice.
                assert_eq!(s.tokens.input, 20);
                assert_eq!(s.tokens.output, 10);
                // deterministic tags: agents, providers, models (+temporal ×2).
                assert!(s
                    .tags
                    .iter()
                    .any(|t| t.root_key == "agents" && t.name == "claude_code"));
                assert!(s
                    .tags
                    .iter()
                    .any(|t| t.root_key == "providers" && t.name == "anthropic"));
                assert!(s.tags.iter().any(|t| t.root_key == "models"));
                assert!(s.tags.iter().any(|t| t.root_key == "time_of_day"));
                assert!(s.tags.iter().any(|t| t.root_key == "cadence"));
                // behavior: 1 user turn, 0 corrections (assistant came after user).
                let b = s.behavior.as_ref().unwrap();
                assert_eq!(b.user_turns, 1);
                assert_eq!(b.correction_count, 0);
                // segment_id is deterministic + prefixed.
                assert!(s.segment_id.starts_with("seg_"));
                assert!(s.abstract_embedding.is_none()); // NoEmbedder → no vector
            }
            BuildOutcome::Held => panic!("expected Ready"),
        }
    }

    #[tokio::test]
    async fn engine_down_holds_the_whole_session() {
        let events = vec![ev(
            "e1",
            "2026-06-01T10:00:00.000Z",
            "user_message",
            Some("do the thing"),
        )];
        let r = resilient(Fake::failing());
        assert_eq!(
            build_for_one_session(&events, &r, &NoEmbedder, &AnsweringRedactor).await,
            BuildOutcome::Held
        );
    }

    #[tokio::test]
    async fn zero_excerpt_slice_is_skipped_not_held() {
        // Pure tool_use / no prose: a healthy engine, but nothing to summarise.
        // The slice is skipped loudly and the batch is Ready (empty), never Held.
        let events = vec![
            ev("e1", "2026-06-01T10:00:00.000Z", "tool_use", None),
            ev("e2", "2026-06-01T10:00:10.000Z", "tool_use", None),
        ];
        let f = Fake::reply("unused");
        let r = resilient(f);
        match build_for_one_session(&events, &r, &NoEmbedder, &AnsweringRedactor).await {
            BuildOutcome::Ready(segs) => assert!(segs.is_empty()),
            BuildOutcome::Held => panic!("0 excerpts must skip, never hold"),
        }
        // The engine was never called (the guard fires before summarize).
        assert_eq!(r.engine().calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tags_cover_project_components_and_tool_mix() {
        let mut e = ev(
            "e1",
            "2026-06-01T10:00:00.000Z",
            "assistant_message",
            Some("shipped the release"),
        );
        e.git = Some(GitContext {
            remote_url: None,
            remote_host: Some("github.com".into()),
            remote_slug: Some("acme/web".into()),
            branch: Some("main".into()),
            slug_source: Some(modelstat_wire::SLUG_SOURCE_GIT_REMOTE.into()),
        });
        e.files_touched = vec!["core/rust/main.rs".into(), "core/rust/lib.rs".into()];
        e.tool_calls = [("Bash".to_string(), 3u64), ("Read".to_string(), 1u64)]
            .into_iter()
            .collect();
        let r = resilient(Fake::reply("Cut a release of acme/web"));
        let BuildOutcome::Ready(segs) =
            build_for_one_session(&[e], &r, &NoEmbedder, &AnsweringRedactor).await
        else {
            panic!("expected Ready");
        };
        let s = &segs[0];
        // A slug read off the repo's own remote is a fact: full confidence.
        assert!(s
            .tags
            .iter()
            .any(|t| t.root_key == "projects" && t.name == "acme/web" && t.confidence == 1.0));
        // `main` used to be tagged `environments: Prod` here — a client-side
        // guess at the server's node names. The branch itself is what ships.
        assert!(!s.tags.iter().any(|t| t.root_key == "environments"));
        // top-2-level dir dedups to a single "core/rust" component.
        let comps: Vec<&str> = s
            .tags
            .iter()
            .filter(|t| t.root_key == "components")
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(comps, vec!["core/rust"]);
        // tool mix: Bash share 0.75, Read share 0.25.
        let bash = s
            .tags
            .iter()
            .find(|t| t.root_key == "tool_calls" && t.name == "Bash")
            .unwrap();
        assert!((bash.confidence - 0.75).abs() < 1e-9);
    }

    #[tokio::test]
    async fn projects_hint_confidence_states_the_slug_provenance_tier() {
        // (slug_source, remote_url, expected confidence): verified tiers ship
        // 1.0, the surviving path guess — and an UNSTATED provenance (SDK
        // producers, pre-marker daemons) — ship 0.5 so the server never mints
        // a workstream node from a guess. A pre-marker event carrying a real
        // remote URL is verified: no guess path ever wrote one. The hint's
        // `reason` carries the marker verbatim (unset stays unset).
        let cases: [(Option<&str>, Option<&str>, f64); 5] = [
            (
                Some(modelstat_wire::SLUG_SOURCE_GIT_REMOTE),
                None,
                modelstat_wire::PROJECT_SLUG_CONFIDENCE_VERIFIED,
            ),
            (
                Some(modelstat_wire::SLUG_SOURCE_REPO_ROOT_DIR),
                None,
                modelstat_wire::PROJECT_SLUG_CONFIDENCE_VERIFIED,
            ),
            (
                None,
                Some("https://github.com/acme/web.git"),
                modelstat_wire::PROJECT_SLUG_CONFIDENCE_VERIFIED,
            ),
            (
                Some(modelstat_wire::SLUG_SOURCE_PATH_SHAPE),
                None,
                modelstat_wire::PROJECT_SLUG_CONFIDENCE_GUESS,
            ),
            (None, None, modelstat_wire::PROJECT_SLUG_CONFIDENCE_GUESS),
        ];
        for (source, remote_url, expected) in cases {
            let mut e = ev(
                "e1",
                "2026-06-01T10:00:00.000Z",
                "assistant_message",
                Some("worked on the thing"),
            );
            e.git = Some(GitContext {
                remote_url: remote_url.map(str::to_string),
                remote_host: None,
                remote_slug: Some("acme/web".into()),
                branch: None,
                slug_source: source.map(str::to_string),
            });
            let r = resilient(Fake::reply("Did the work"));
            let BuildOutcome::Ready(segs) =
                build_for_one_session(&[e], &r, &NoEmbedder, &AnsweringRedactor).await
            else {
                panic!("expected Ready");
            };
            let hint = segs[0]
                .tags
                .iter()
                .find(|t| t.root_key == "projects")
                .expect("projects hint present");
            assert_eq!(hint.confidence, expected, "slug_source {source:?}");
            assert_eq!(
                hint.reason.as_deref(),
                source,
                "reason is the marker verbatim, slug_source {source:?}"
            );
        }
    }

    #[tokio::test]
    async fn projects_hint_reads_the_first_verified_slug_in_the_slice() {
        // A slice can open on a guessed context (cwd outside the repo) and
        // reach the real repo a turn later — the verified slug wins the hint.
        let guess = |id: &str, ts: &str, slug: &str| {
            let mut e = ev(id, ts, "assistant_message", Some("worked on the thing"));
            e.git = Some(GitContext {
                remote_url: None,
                remote_host: None,
                remote_slug: Some(slug.into()),
                branch: None,
                slug_source: Some(modelstat_wire::SLUG_SOURCE_PATH_SHAPE.into()),
            });
            e
        };
        let mut verified = ev(
            "e2",
            "2026-06-01T10:01:00.000Z",
            "assistant_message",
            Some("still working"),
        );
        verified.git = Some(GitContext {
            remote_url: None,
            remote_host: Some("github.com".into()),
            remote_slug: Some("acme/web".into()),
            branch: None,
            slug_source: Some(modelstat_wire::SLUG_SOURCE_GIT_REMOTE.into()),
        });
        let r = resilient(Fake::reply("Did the work"));
        let BuildOutcome::Ready(segs) = build_for_one_session(
            &[
                guess("e1", "2026-06-01T10:00:00.000Z", "guessed/subdir"),
                verified,
            ],
            &r,
            &NoEmbedder,
            &AnsweringRedactor,
        )
        .await
        else {
            panic!("expected Ready");
        };
        let hint = segs[0]
            .tags
            .iter()
            .find(|t| t.root_key == "projects")
            .expect("projects hint present");
        assert_eq!(hint.name, "acme/web");
        assert_eq!(
            hint.confidence,
            modelstat_wire::PROJECT_SLUG_CONFIDENCE_VERIFIED
        );
        assert_eq!(
            hint.reason.as_deref(),
            Some(modelstat_wire::SLUG_SOURCE_GIT_REMOTE)
        );

        // With no verified slug anywhere, the FIRST slug still wins as before.
        let r = resilient(Fake::reply("Did the work"));
        let BuildOutcome::Ready(segs) = build_for_one_session(
            &[
                guess("e1", "2026-06-01T10:00:00.000Z", "guessed/subdir"),
                guess("e3", "2026-06-01T10:01:00.000Z", "other/guess"),
            ],
            &r,
            &NoEmbedder,
            &AnsweringRedactor,
        )
        .await
        else {
            panic!("expected Ready");
        };
        let hint = segs[0]
            .tags
            .iter()
            .find(|t| t.root_key == "projects")
            .expect("projects hint present");
        assert_eq!(hint.name, "guessed/subdir");
        assert_eq!(
            hint.confidence,
            modelstat_wire::PROJECT_SLUG_CONFIDENCE_GUESS
        );
        assert_eq!(
            hint.reason.as_deref(),
            Some(modelstat_wire::SLUG_SOURCE_PATH_SHAPE)
        );
    }

    #[tokio::test]
    async fn corrections_and_frustration_are_counted() {
        // user → assistant → user (a re-prompt right after the assistant) → 1 correction.
        let events = vec![
            ev(
                "e1",
                "2026-06-01T10:00:00.000Z",
                "user_message",
                Some("try X"),
            ),
            ev(
                "e2",
                "2026-06-01T10:00:05.000Z",
                "assistant_message",
                Some("did X"),
            ),
            ev(
                "e3",
                "2026-06-01T10:00:10.000Z",
                "user_message",
                Some("no, do Y"),
            ),
        ];
        let r = resilient(Fake::reply("Iterated on X then Y"));
        let BuildOutcome::Ready(segs) =
            build_for_one_session(&events, &r, &NoEmbedder, &AnsweringRedactor).await
        else {
            panic!("expected Ready");
        };
        let b = segs[0].behavior.as_ref().unwrap();
        assert_eq!(b.user_turns, 2);
        assert_eq!(b.correction_count, 1);
        // The counts ship; the score does not. Omitted, not zeroed — a zero
        // would read as "measured, and calm".
        assert_eq!(b.frustration, None);
    }

    #[tokio::test]
    async fn full_embedder_attaches_a_384_dim_vector() {
        let events = vec![ev(
            "e1",
            "2026-06-01T10:00:00.000Z",
            "user_message",
            Some("embed me"),
        )];
        let r = resilient(Fake::reply("Did the embeddable work"));
        let BuildOutcome::Ready(segs) =
            build_for_one_session(&events, &r, &FixedEmbedder, &AnsweringRedactor).await
        else {
            panic!("expected Ready");
        };
        assert_eq!(
            segs[0].abstract_embedding.as_ref().unwrap().len(),
            EMBED_DIM
        );
    }

    #[test]
    fn temporal_buckets_match_the_frozen_boundaries() {
        assert_eq!(time_of_day_bucket(5), "Morning");
        assert_eq!(time_of_day_bucket(11), "Morning");
        assert_eq!(time_of_day_bucket(12), "Midday");
        assert_eq!(time_of_day_bucket(16), "Midday");
        assert_eq!(time_of_day_bucket(17), "Evening");
        assert_eq!(time_of_day_bucket(20), "Evening");
        assert_eq!(time_of_day_bucket(21), "Night");
        assert_eq!(time_of_day_bucket(3), "Night");
        assert_eq!(cadence_bucket(0), "Weekend"); // Sun
        assert_eq!(cadence_bucket(6), "Weekend"); // Sat
        assert_eq!(cadence_bucket(5), "Friday");
        assert_eq!(cadence_bucket(3), "Weekday");
    }

    #[test]
    fn the_local_reading_agrees_with_the_buckets_derived_from_it() {
        // Runs in whatever zone the machine is in — the assertion is the
        // RELATIONSHIP, which is what shipping the reading buys: the server can
        // re-derive every bucket the daemon emitted, and cut differently later.
        let local = local_time_of(1_764_590_400_000).expect("a real instant reads");
        assert!(local.hour < 24);
        assert!(local.weekday < 7);
        assert!((-12 * 60..=14 * 60).contains(&local.utc_offset_minutes));
        let hints = temporal_hints(local);
        assert_eq!(hints[0].name, time_of_day_bucket(local.hour));
        assert_eq!(hints[1].name, cadence_bucket(local.weekday));
    }

    #[test]
    fn components_dedupe_and_cap() {
        let mk = |files: Vec<&str>| {
            let mut e = ev("e", "2026-06-01T10:00:00Z", "assistant_message", None);
            e.files_touched = files.into_iter().map(str::to_string).collect();
            e
        };
        let a = mk(vec!["src/app/x.ts", "src/app/y.ts", "docs/readme.md"]);
        let refs: Vec<&RawEvent> = vec![&a];
        // src/app dedups to one; docs/readme.md → "docs/readme.md".
        assert_eq!(
            components_from_slice(&refs),
            vec!["src/app", "docs/readme.md"]
        );
    }

    #[test]
    fn excerpt_sampling_keeps_first_and_last() {
        let evs: Vec<RawEvent> = (0..10)
            .map(|i| {
                ev(
                    "e",
                    "2026-06-01T10:00:00Z",
                    "user_message",
                    Some(Box::leak(format!("msg {i}").into_boxed_str())),
                )
            })
            .collect();
        let refs: Vec<&RawEvent> = evs.iter().collect();
        let out = sample_and_redact_excerpts(&refs);
        assert!(out.len() >= 2 && out.len() <= 5);
        assert_eq!(out.first().unwrap(), "msg 0");
        assert_eq!(out.last().unwrap(), "msg 9");
    }

    #[test]
    fn no_content_yields_no_excerpts() {
        let a = ev("e", "2026-06-01T10:00:00Z", "tool_use", None);
        let refs: Vec<&RawEvent> = vec![&a];
        assert!(sample_and_redact_excerpts(&refs).is_empty());
    }

    fn seg(session: &str, started_at: &str, abstract_text: &str, project: Option<&str>) -> Segment {
        let mut tags = Vec::new();
        if let Some(p) = project {
            tags.push(hint("projects", p, 1.0));
        }
        Segment {
            segment_id: "seg_x".into(),
            session_id: session.into(),
            agent: "claude_code".into(),
            started_at: started_at.into(),
            ended_at: started_at.into(),
            r#abstract: abstract_text.into(),
            tokens: TokenUsage::default(),
            tags,
            redaction: RedactionReport::default(),
            source_event_ids: vec![],
            abstract_embedding: None,
            behavior: None,
            user_intent: None,
            local_time: None,
        }
    }

    #[tokio::test]
    async fn titles_from_a_healthy_titler() {
        let segs = vec![
            seg(
                "s1",
                "2026-06-01T10:00:00Z",
                "Refactored the auth middleware",
                Some("acme/web"),
            ),
            seg("s1", "2026-06-01T10:05:00Z", "Added retry logic", None),
        ];
        let engine = Fake::reply("Auth Middleware Refactor");
        let titles = build_session_titles(&segs, &engine).await;
        assert_eq!(
            titles.get("s1").map(String::as_str),
            Some("Auth Middleware Refactor")
        );
    }

    #[tokio::test]
    async fn falls_back_to_first_sentence_when_titler_fails() {
        let segs = vec![seg(
            "s1",
            "2026-06-01T10:00:00Z",
            "Fixed the null deref. Then shipped it.",
            None,
        )];
        let engine = Fake::failing();
        let titles = build_session_titles(&segs, &engine).await;
        // Deterministic fallback = first sentence of the first abstract, sanitised.
        assert_eq!(
            titles.get("s1").map(String::as_str),
            Some("Fixed the null deref")
        );
    }

    #[tokio::test]
    async fn session_with_only_empty_abstracts_is_omitted() {
        let segs = vec![seg("s1", "2026-06-01T10:00:00Z", "", None)];
        let engine = Fake::reply("won't be used");
        assert!(build_session_titles(&segs, &engine).await.is_empty());
    }

    /// Every mode has a REDACTOR, and none of them ship without it. Local mode was
    /// the quiet exception: it redacts best-effort, summarises on this machine, and
    /// then uploads the abstract — so a turn the redactor could not read became a
    /// summary of unscrubbed text, sent anyway. It holds now, like cloud does.
    #[tokio::test]
    async fn local_mode_holds_when_the_redactor_cannot_answer() {
        let events = vec![
            ev(
                "e1",
                "2026-07-16T10:00:00.000Z",
                "message",
                Some("ship the thing"),
            ),
            ev(
                "e2",
                "2026-07-16T10:01:00.000Z",
                "message",
                Some("shipped it"),
            ),
        ];
        let r = ResilientSummarizer::with_cooldown(Fake::reply("did the thing"), Duration::ZERO);
        // The engine is healthy; only the redactor is missing.
        assert!(
            matches!(
                build_for_one_session(&events, &r, &NoEmbedder, &UnavailableRedactor).await,
                BuildOutcome::Held
            ),
            "an abstract is egress too — no scrub, no upload"
        );

        // With a redactor that answers, the same session builds.
        assert!(matches!(
            build_for_one_session(&events, &r, &NoEmbedder, &AnsweringRedactor).await,
            BuildOutcome::Ready(_)
        ));
    }
}
