//! The six LLM passes (feature §9, §18) — a port of `pipeline/passes.ts` + the
//! parsers in the sibling TS modules (`cognition.ts`, `title.ts`, `redaction.ts`,
//! `script-summary.ts`, `session-metadata.ts`).
//!
//! Each pass is thin: guard the input → build the frozen prompt → run one
//! completion over the summarizer protocol → parse/slice. The **summarize** pass
//! is REQUIRED and goes through the resilient hold-and-retry wrapper (§9.4 — held,
//! never degraded, on failure). The other five are best-effort enrichment over an
//! already-redacted abstract: any failure ⇒ no signal (the segment ships without
//! it), and the redaction backstop is fail-SAFE (returns the command unchanged).

use std::collections::HashSet;

use modelstat_redact::redact;
use modelstat_sumclient::CompleteRequest;
use modelstat_wire::TaxonomyHintRooted;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::prompts::{
    build_cognition_user_prompt, build_link_extract_user_prompt, build_script_summary_user_prompt,
    COGNITION_MAX_TOKENS, COGNITION_SYSTEM_PROMPT, COGNITION_TEMPERATURE, LINK_EXTRACT_MAX_TOKENS,
    LINK_EXTRACT_SYSTEM_PROMPT, LINK_EXTRACT_TEMPERATURE, MAX_COGNITION_TAGS_PER_FIELD,
    MAX_COGNITION_TAG_CHARS, REDACT_MAX_TOKENS, REDACT_SYSTEM_PROMPT, REDACT_TEMPERATURE,
    SCRIPT_SUMMARY_OUTPUT_MAX_CHARS, SCRIPT_SUMMARY_SYSTEM_PROMPT, SCRIPT_SUMMARY_TEMPERATURE,
    SUMMARISER_MAX_TOKENS, SUMMARISER_SYSTEM_PROMPT, SUMMARISER_TEMPERATURE, SUMMARISER_TOP_K,
    TITLER_MAX_TOKENS, TITLER_SYSTEM_PROMPT, TITLER_TEMPERATURE, TITLE_MAX_CHARS,
    TITLER_ABSTRACT_SLICE_CHARS,
};
use crate::resilient::{ResilientSummarizer, SummarizeOutcome, Summarizer};

/// Token headroom added to an auxiliary pass's answer budget so a reasoning model
/// can emit its `<think>` block before the (tiny) answer (§18 thinking.ts).
pub const THINKING_HEADROOM_TOKENS: u32 = 400;
/// The summarize pass slices the model output to this; the caller may then add
/// context up to the 400-char abstract cap (§18 "sliced to 240 then capped 400").
pub const SUMMARY_OUTPUT_MAX_CHARS: usize = 240;

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── Summarize (required, resilient) ──────────────────────────────────────────

/// The summarize pass. `user_prompt` is the pre-built facts+excerpts message.
/// Goes through the resilient wrapper: `Done(abstract≤240)` or `Held`.
pub async fn summarize<S: Summarizer>(
    r: &ResilientSummarizer<S>,
    user_prompt: &str,
) -> SummarizeOutcome {
    let req = CompleteRequest {
        system: SUMMARISER_SYSTEM_PROMPT.to_string(),
        user: user_prompt.to_string(),
        temperature: SUMMARISER_TEMPERATURE,
        max_tokens: SUMMARISER_MAX_TOKENS,
        top_k: Some(SUMMARISER_TOP_K),
    };
    match r.summarize(&req).await {
        SummarizeOutcome::Done(text) => {
            SummarizeOutcome::Done(take_chars(&text, SUMMARY_OUTPUT_MAX_CHARS))
        }
        SummarizeOutcome::Held => SummarizeOutcome::Held,
    }
}

// ── Cognition (best-effort) ──────────────────────────────────────────────────

/// Mood/mind/posture tags for the segment, from the already-redacted abstract.
/// A trivial abstract (<12 chars) or any failure ⇒ None (no completion run).
pub async fn cognition<S: Summarizer>(s: &S, abstract_text: &str) -> Option<CognitionTags> {
    if abstract_text.trim().chars().count() < 12 {
        return None;
    }
    let req = CompleteRequest {
        system: COGNITION_SYSTEM_PROMPT.to_string(),
        user: build_cognition_user_prompt(abstract_text),
        temperature: COGNITION_TEMPERATURE,
        max_tokens: COGNITION_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
        top_k: None,
    };
    match s.complete(&req).await {
        Ok(out) => parse_cognition_reply(&out),
        Err(_) => None,
    }
}

/// Best-effort session title — the RAW model reply (the caller sanitises via
/// [`sanitise_title`] and falls back deterministically on None).
pub async fn title<S: Summarizer>(s: &S, user_prompt: &str) -> Option<String> {
    let req = CompleteRequest {
        system: TITLER_SYSTEM_PROMPT.to_string(),
        user: user_prompt.to_string(),
        temperature: TITLER_TEMPERATURE,
        max_tokens: TITLER_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
        top_k: None,
    };
    match s.complete(&req).await {
        Ok(out) if !out.is_empty() => Some(out),
        _ => None,
    }
}

/// Best-effort link extraction — the RAW model reply (free text the caller
/// re-parses deterministically in M4's session-metadata builder). None on failure.
pub async fn link_extract<S: Summarizer>(s: &S, abstracts: &[String]) -> Option<String> {
    if abstracts.is_empty() {
        return None;
    }
    let req = CompleteRequest {
        system: LINK_EXTRACT_SYSTEM_PROMPT.to_string(),
        user: build_link_extract_user_prompt(abstracts),
        temperature: LINK_EXTRACT_TEMPERATURE,
        max_tokens: LINK_EXTRACT_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
        top_k: None,
    };
    match s.complete(&req).await {
        Ok(out) if !out.is_empty() => Some(out),
        _ => None,
    }
}

/// Best-effort one-sentence summary of what running a script does. Empty content
/// or any failure ⇒ None (the call still ships its redacted command).
pub async fn script_summary<S: Summarizer>(
    s: &S,
    reference: &str,
    content: &str,
) -> Option<String> {
    if content.trim().is_empty() {
        return None;
    }
    let req = CompleteRequest {
        system: SCRIPT_SUMMARY_SYSTEM_PROMPT.to_string(),
        user: build_script_summary_user_prompt(reference, content),
        temperature: SCRIPT_SUMMARY_TEMPERATURE,
        max_tokens: SCRIPT_SUMMARY_MAX_TOKENS + THINKING_HEADROOM_TOKENS,
        top_k: None,
    };
    match s.complete(&req).await {
        Ok(out) => {
            let one_line = collapse_ws(&out);
            if one_line.is_empty() {
                None
            } else {
                Some(take_chars(&one_line, SCRIPT_SUMMARY_OUTPUT_MAX_CHARS))
            }
        }
        Err(_) => None,
    }
}

/// Script-summary answer budget (matches the TS `SCRIPT_SUMMARY_MAX_TOKENS`).
pub const SCRIPT_SUMMARY_MAX_TOKENS: u32 = 200;

// ── Redaction backstop (layer 3, local-mode only, fail-safe) ─────────────────

/// The LLM redaction backstop (§9.5): asks the model to NAME any remaining secret
/// substrings, then deterministically replaces each verbatim occurrence. It can
/// only ever ADD a redaction of a string that genuinely appears — never reword,
/// invent, or leak. Returns `(text, count)`; on the prefilter miss or ANY failure
/// the command is returned UNCHANGED (fail-safe).
pub async fn redact_backstop<S: Summarizer>(s: &S, command: &str) -> (String, u64) {
    if !should_deep_redact(command) {
        return (command.to_string(), 0);
    }
    let req = CompleteRequest {
        // The system prompt says "you are given a single shell command", so the
        // user turn is the (partly-redacted) command itself.
        system: REDACT_SYSTEM_PROMPT.to_string(),
        user: command.to_string(),
        temperature: REDACT_TEMPERATURE,
        max_tokens: REDACT_MAX_TOKENS,
        top_k: None,
    };
    match s.complete(&req).await {
        Ok(out) => {
            let candidates = parse_redact_reply(&out);
            apply_llm_redactions(command, &candidates)
        }
        Err(_) => (command.to_string(), 0), // fail-safe: unchanged
    }
}

// ── Cognition parsing (cognition.ts) ─────────────────────────────────────────

/// Sanitised mood/mind/posture tags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CognitionTags {
    pub emotions: Vec<String>,
    pub meta: Vec<String>,
    pub posture: Vec<String>,
}

/// Parse + sanitise a cognition reply, tolerating ```json fences``` / leading
/// prose via a balanced-brace scan. None when nothing parses.
pub fn parse_cognition_reply(text: &str) -> Option<CognitionTags> {
    let json = extract_first_json_object(text)?;
    let parsed: Value = serde_json::from_str(&json).ok()?;
    if !parsed.is_object() {
        return None;
    }
    Some(CognitionTags {
        emotions: sanitise_tags(parsed.get("emotions")),
        meta: sanitise_tags(parsed.get("meta")),
        posture: sanitise_tags(parsed.get("posture")),
    })
}

/// Lowercase → NFKD → keep `[a-z0-9-]` → collapse/trim hyphens → cap 24 chars,
/// dedupe, cap 3 per field.
pub fn sanitise_tags(raw: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(arr)) = raw else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for t in arr {
        let Some(t) = t.as_str() else { continue };
        let lowered: String = t.to_lowercase().nfkd().collect();
        let filtered: String = lowered
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
            .collect();
        let collapsed = collapse_hyphens(&filtered);
        let trimmed = collapsed.trim_matches('-');
        let cleaned: String = trimmed.chars().take(MAX_COGNITION_TAG_CHARS).collect();
        if cleaned.is_empty() || seen.contains(&cleaned) {
            continue;
        }
        seen.insert(cleaned.clone());
        out.push(cleaned);
        if out.len() >= MAX_COGNITION_TAGS_PER_FIELD {
            break;
        }
    }
    out
}

fn collapse_hyphens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev = false;
    for c in s.chars() {
        if c == '-' {
            if !prev {
                out.push('-');
            }
            prev = true;
        } else {
            out.push(c);
            prev = false;
        }
    }
    out
}

/// The PRIMARY (first) mood/mind/posture tag of each field as segment taxonomy
/// hints (`mood`/`mind`/`posture`, capitalised, confidence 0.7). Port of
/// `cognitionHints`. Empty when there's nothing to emit.
pub fn cognition_hints(c: &CognitionTags) -> Vec<TaxonomyHintRooted> {
    let mut out = Vec::new();
    let mut push = |root_key: &str, first: Option<&String>| {
        if let Some(name) = first {
            out.push(TaxonomyHintRooted {
                root_key: root_key.to_string(),
                name: capitalize(name),
                confidence: 0.7,
                reason: None,
            });
        }
    };
    push("mood", c.emotions.first());
    push("mind", c.meta.first());
    push("posture", c.posture.first());
    out
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `[Mood: …] [Mind: …] [Stance: …]` suffix, "" when empty.
pub fn format_cognition_suffix(c: &CognitionTags) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !c.emotions.is_empty() {
        parts.push(format!("[Mood: {}]", c.emotions.join(", ")));
    }
    if !c.meta.is_empty() {
        parts.push(format!("[Mind: {}]", c.meta.join(", ")));
    }
    if !c.posture.is_empty() {
        parts.push(format!("[Stance: {}]", c.posture.join(", ")));
    }
    parts.join(" ")
}

/// Extract the FIRST balanced `{…}` block (robust to fences / trailing prose).
fn extract_first_json_object(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.iter().position(|&c| c == '{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for i in start..chars.len() {
        let ch = chars[i];
        if in_str {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if ch == '"' {
            in_str = true;
            continue;
        }
        if ch == '{' {
            depth += 1;
        } else if ch == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(chars[start..=i].iter().collect());
            }
        }
    }
    None
}

// ── Title sanitising (title.ts) ──────────────────────────────────────────────

/// Strip the cognition suffix the pipeline appends to abstracts before feeding
/// them to the titler.
pub fn strip_cognition_suffix(abstract_text: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\s*\[(?:Mood|Mind|Stance):[^\]]*\]").unwrap()
    });
    re.replace_all(abstract_text, "").trim().to_string()
}

/// Clean the raw titler reply: first non-empty line, collapse whitespace, drop
/// surrounding quotes + a trailing period/bang, re-redact, cap 80. "" when empty.
pub fn sanitise_title(raw: &str) -> String {
    use std::sync::OnceLock;
    static FENCE: OnceLock<regex::Regex> = OnceLock::new();
    let fence = FENCE.get_or_init(|| regex::Regex::new(r"(?i)```[a-z]*\n?").unwrap());
    let defenced = fence.replace_all(raw, "");
    let first_line = defenced
        .split(['\n', '\r'])
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let collapsed = collapse_ws(first_line);
    let is_quote = |c: char| matches!(c, '"' | '\'' | '`' | '\u{201c}' | '\u{201d}');
    let t = collapsed
        .trim_start_matches(is_quote)
        .trim_end_matches(is_quote)
        .trim_end_matches(['.', '!'])
        .trim();
    if t.is_empty() {
        return String::new();
    }
    let redacted = redact(t, None).text;
    take_chars(redacted.trim(), TITLE_MAX_CHARS)
}

/// Build the title user prompt (`title.ts::buildTitleUserPrompt`).
pub fn build_title_user_prompt(abstracts: &[String], facts: Option<&str>) -> String {
    let lines = abstracts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            format!(
                "  [part {}] {}",
                i + 1,
                take_chars(&collapse_ws(a), TITLER_ABSTRACT_SLICE_CHARS)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let facts_prefix = match facts.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!("Session context: {f}.\n\n"),
        None => String::new(),
    };
    format!("{facts_prefix}Summaries of the session's parts (chronological):\n{lines}\n\nWrite the title.")
}

/// Deterministic title — the first segment's abstract (the session's intent),
/// cognition-stripped, cut at the first sentence boundary, sanitised. Never an
/// LLM call. Port of `fallbackTitle`.
pub fn fallback_title(abstracts: &[String]) -> String {
    let Some(first) = abstracts.iter().find(|a| !a.trim().is_empty()) else {
        return String::new();
    };
    sanitise_title(first_sentence(first))
}

/// Everything up to (and including) the first sentence-ending `.`/`!`/`?` that is
/// followed by whitespace (JS `split(/(?<=[.!?])\s/, 1)[0]`). Rust regex has no
/// lookbehind, so this is a manual scan.
fn first_sentence(s: &str) -> &str {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    for i in 0..chars.len() {
        if matches!(chars[i].1, '.' | '!' | '?') {
            if let Some(&(next_byte, next_char)) = chars.get(i + 1) {
                if next_char.is_whitespace() {
                    return &s[..next_byte];
                }
            }
        }
    }
    s
}

/// Evenly sample up to `max` abstracts keeping first + last (port of
/// `sampleAbstracts`).
pub fn sample_abstracts(abstracts: &[String], max: usize) -> Vec<String> {
    if abstracts.len() <= max || max == 0 {
        return abstracts.to_vec();
    }
    let last = abstracts.len() - 1;
    let mut picks: std::collections::BTreeSet<usize> = [0, last].into_iter().collect();
    let mut i = 1;
    while picks.len() < max && i <= last + 2 {
        // Round(i*(len-1)/(max-1)) — matches the JS spacing (half rounds up).
        let idx = ((i * last) as f64 / (max - 1) as f64).round() as usize;
        picks.insert(idx.min(last));
        i += 1;
    }
    picks.into_iter().map(|i| abstracts[i].clone()).collect()
}

// ── Redaction backstop parsing (redaction.ts) ────────────────────────────────

const LLM_REDACTION_MARKER: &str = "[REDACTED:llm]";
const MIN_CANDIDATE_CHARS: usize = 8;

fn is_safe_word(s: &str) -> bool {
    matches!(
        s,
        "production"
            | "staging"
            | "localhost"
            | "endpoint"
            | "database"
            | "password"
            | "secret"
            | "token"
            | "credential"
            | "[redacted"
    )
}

/// Cheap pre-filter: only spend a model call on commands that *could* still carry
/// a secret the earlier layers missed.
pub fn should_deep_redact(text: &str) -> bool {
    use std::sync::OnceLock;
    if text.is_empty() {
        return false;
    }
    static CUE: OnceLock<regex::Regex> = OnceLock::new();
    static BLOB: OnceLock<regex::Regex> = OnceLock::new();
    let cue = CUE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)[=]|--|://|@|(?-u:\b)bearer(?-u:\b)|token|secret|password|passwd|credential|api[_-]?key|private[_-]?key",
        )
        .unwrap()
    });
    if cue.is_match(text) {
        return true;
    }
    let blob = BLOB.get_or_init(|| regex::Regex::new(r"[A-Za-z0-9/+_-]{20,}").unwrap());
    blob.is_match(text)
}

/// Parse the model's reply into candidate secret substrings.
pub fn parse_redact_reply(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in raw.split('\n') {
        let trimmed = line.trim();
        let s = trimmed
            .trim_start_matches(['"', '\'', '`'])
            .trim_end_matches(['"', '\'', '`']);
        if s.is_empty() || s.eq_ignore_ascii_case("NONE") {
            continue;
        }
        if s.chars().count() < MIN_CANDIDATE_CHARS {
            continue;
        }
        if is_safe_word(&s.to_lowercase()) {
            continue;
        }
        if s.starts_with("[REDACTED") {
            continue;
        }
        if seen.contains(s) {
            continue;
        }
        seen.insert(s.to_string());
        out.push(s.to_string());
    }
    out
}

/// Replace every verbatim occurrence of each candidate (longest-first) with the
/// LLM marker. Only strings that actually appear are applied.
pub fn apply_llm_redactions(text: &str, candidates: &[String]) -> (String, u64) {
    let mut sorted: Vec<&String> = candidates.iter().collect();
    sorted.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_string();
    let mut count = 0u64;
    for cand in sorted {
        if !out.contains(cand.as_str()) {
            continue;
        }
        let before = out.clone();
        out = out.replace(cand.as_str(), LLM_REDACTION_MARKER);
        if out != before {
            count += 1;
        }
    }
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modelstat_sumclient::SumError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A fake engine that returns a fixed reply (or fails).
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
                Err(SumError::Http(500))
            } else {
                Ok(self.reply.clone())
            }
        }
    }

    #[tokio::test]
    async fn summarize_slices_to_240_via_resilient() {
        let long = "x".repeat(500);
        let r = ResilientSummarizer::with_cooldown(Fake::reply(&long), Duration::ZERO);
        match summarize(&r, "facts + excerpts").await {
            SummarizeOutcome::Done(t) => assert_eq!(t.chars().count(), 240),
            SummarizeOutcome::Held => panic!("expected Done"),
        }
    }

    #[tokio::test]
    async fn summarize_holds_on_engine_error() {
        let r = ResilientSummarizer::with_cooldown(Fake::failing(), Duration::ZERO);
        assert_eq!(summarize(&r, "facts").await, SummarizeOutcome::Held);
    }

    #[tokio::test]
    async fn cognition_parses_fenced_json() {
        let fake = Fake::reply(
            "```json\n{\"emotions\":[\"Frustrated!\",\"frustrated\"],\"meta\":[\"in-flow\"],\"posture\":[]}\n```",
        );
        let tags = cognition(&fake, "a long enough abstract about work").await.unwrap();
        // Dedup + sanitise: "Frustrated!" → "frustrated", dup dropped.
        assert_eq!(tags.emotions, vec!["frustrated"]);
        assert_eq!(tags.meta, vec!["in-flow"]);
        assert!(tags.posture.is_empty());
    }

    #[tokio::test]
    async fn cognition_skips_trivial_abstract_without_calling() {
        let fake = Fake::reply("{}");
        assert!(cognition(&fake, "short").await.is_none());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn suffix_and_strip_roundtrip() {
        let c = CognitionTags {
            emotions: vec!["frustrated".into(), "curious".into()],
            meta: vec!["debugging".into()],
            posture: vec![],
        };
        let suffix = format_cognition_suffix(&c);
        assert_eq!(suffix, "[Mood: frustrated, curious] [Mind: debugging]");
        assert_eq!(
            strip_cognition_suffix(&format!("Fixed the bug {suffix}")),
            "Fixed the bug"
        );
    }

    #[test]
    fn cognition_hints_emit_primary_capitalised() {
        let c = CognitionTags {
            emotions: vec!["frustrated".into(), "curious".into()],
            meta: vec!["in-flow".into()],
            posture: vec![],
        };
        let h = cognition_hints(&c);
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].root_key.as_str(), h[0].name.as_str(), h[0].confidence), ("mood", "Frustrated", 0.7));
        assert_eq!((h[1].root_key.as_str(), h[1].name.as_str()), ("mind", "In-flow"));
    }

    #[test]
    fn fallback_title_cuts_first_sentence() {
        assert_eq!(
            fallback_title(&["".into(), "Fixed the bug. Then did more work.".into()]),
            "Fixed the bug"
        );
        assert_eq!(fallback_title(&[]), "");
    }

    #[test]
    fn sample_abstracts_keeps_first_last_and_count() {
        let a: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        let s = sample_abstracts(&a, 3);
        assert_eq!(s.len(), 3);
        assert_eq!(s.first().unwrap(), "0");
        assert_eq!(s.last().unwrap(), "9");
        assert_eq!(sample_abstracts(&a, 20), a); // ≤ max → all
    }

    #[test]
    fn title_sanitiser_cleans_quotes_fences_and_period() {
        assert_eq!(sanitise_title("```\n\"Auth middleware refactor.\"\n```"), "Auth middleware refactor");
        assert_eq!(sanitise_title("A paragraph\nwith a second line"), "A paragraph");
        assert_eq!(sanitise_title(""), "");
    }

    #[test]
    fn redact_backstop_prefilter_and_apply() {
        assert!(!should_deep_redact("git status"));
        assert!(should_deep_redact("curl -H 'Authorization: Bearer abc' https://x"));
        let candidates = parse_redact_reply("NONE\nsk_live_abcdef123456\nprod\ntoken\n");
        // "prod" (<8) and "token" (safe word) dropped; only the long secret kept.
        assert_eq!(candidates, vec!["sk_live_abcdef123456"]);
        let (out, n) = apply_llm_redactions("run sk_live_abcdef123456 now", &candidates);
        assert_eq!(out, "run [REDACTED:llm] now");
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn redact_backstop_is_fail_safe() {
        let fake = Fake::failing();
        let (out, n) = redact_backstop(&fake, "curl -H 'Authorization: Bearer sekret' x").await;
        assert_eq!(out, "curl -H 'Authorization: Bearer sekret' x"); // unchanged
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn script_summary_collapses_and_caps() {
        let fake = Fake::reply(&format!("  It  does\n{}  ", "y".repeat(300)));
        let out = script_summary(&fake, "./x.sh", "echo hi").await.unwrap();
        assert!(out.chars().count() <= SCRIPT_SUMMARY_OUTPUT_MAX_CHARS);
        assert!(out.starts_with("It does"));
    }
}
