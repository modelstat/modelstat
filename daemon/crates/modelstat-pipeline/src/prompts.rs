//! The six frozen system prompts + their constants (feature §18), copied
//! VERBATIM from the TS pipeline (`packages/daemon-core/src/pipeline/`). The
//! collector owns every prompt and sends it per-request over the summarizer
//! protocol; the engine strips `<think>`. Changing any wording here shifts model
//! output and REQUIRES a PROCESSING_VERSION bump.
//!
//! Rev 2 note: `SUMMARISER_MAX_TOKENS` is 1024 (thinking headroom, §18) — the
//! Qwen3.5-4B engine reasons in `<think>` before the sentence, which the engine
//! strips; this supersedes the old TS value of 180.

// ── Summarizer (pipeline/prompts.ts) ─────────────────────────────────────────

pub const SUMMARISER_SYSTEM_PROMPT: &str = "You summarise an AI coding session in 1-2 sentences, ≤ 400 characters. Name the CONCRETE work: the action taken, WHAT it acted on, and the specific target — the repository (e.g. acme/web), branch, service, or component — whenever the session facts or excerpts identify it. Good: \"Triggered a GitHub Actions release for acme/web\"; \"Fixed a null dereference in the auth middleware of api-gateway\". Lead with an outcome verb and pack in concrete domain keywords (frameworks, features, decisions). Base the substance on the excerpts when present, else the metadata. Never quote excerpts verbatim. No PII, no secrets, no API keys, no absolute file paths. Reply with only the summary.";

pub const SUMMARISER_TEMPERATURE: f64 = 0.2;
pub const SUMMARISER_TOP_K: u32 = 3;
/// Rev 2: 1024 = thinking headroom (§18), supersedes the TS 180.
pub const SUMMARISER_MAX_TOKENS: u32 = 1024;
/// The pipeline slices the abstract to this after the model returns.
pub const ABSTRACT_OUTPUT_MAX_CHARS: usize = 400;
/// The wire storage cap on `abstract`.
pub const ABSTRACT_MAX_CHARS: usize = 512;

// ── Cognition (pipeline/cognition.ts) ────────────────────────────────────────

pub const COGNITION_SYSTEM_PROMPT: &str = "You read a one-sentence summary of an AI-coding work session and identify the user's emotional state, mental mode, and working stance. Output JSON only — first character of reply is `{`. Schema: {\"emotions\":[],\"meta\":[],\"posture\":[]}. emotions: ≤ 3 short lowercase MOOD tags — how the user FEELS — such as frustrated, curious, excited, calm, confused, anxious, satisfied, proud, happy, worried, disappointed, overwhelmed, confident. meta: ≤ 3 short lowercase MENTAL-MODE tags — HOW the user is THINKING, never what they are doing. Valid examples: focused, scattered, in-flow, deliberate, hurried, stuck, open, exploratory, methodical, distracted. DO NOT emit ACTIVITY verbs (debugging, refactoring, designing, reviewing, deploying, planning, documenting, implementing) under meta — those describe the WORK, not the MIND. If the only candidate tag would be an activity verb, return [] for meta instead. posture: ≤ 2 short lowercase WORKING-STANCE tags — the user's risk appetite and how they treat the work, inferred from how boldly or carefully they move — such as ship-it, yolo, direct-to-prod, cautious, careful, methodical, questioning, skeptical, demanding, easygoing, trusting. Each tag ≤ 24 chars, single word or hyphenated, no punctuation. Only emit a tag if the summary gives clear evidence — return [] for any field when unsure. Do not invent emotions, mental modes, or stances the user did not display. No prose, no markdown.";

pub const COGNITION_MAX_TOKENS: u32 = 80;
pub const COGNITION_TEMPERATURE: f64 = 0.2;
pub const MAX_COGNITION_TAGS_PER_FIELD: usize = 3;
pub const MAX_COGNITION_TAG_CHARS: usize = 24;

// ── Titler (pipeline/title.ts) ───────────────────────────────────────────────

pub const TITLER_SYSTEM_PROMPT: &str = "You write a short TITLE for an AI coding session, given one-sentence summaries of its parts in chronological order. The title names what the session was about at a high level, in 3-8 words. If the session clearly spans more than one theme, name the top themes (at most 3) separated by ' · '. Use concrete domain keywords (features, components, subsystems). No narration, no filler words like 'session' or 'work on', no quotes, no trailing period, no PII, no code literals, no file paths. Reply with only the title.";

pub const TITLER_MAX_TOKENS: u32 = 60;
pub const TITLER_TEMPERATURE: f64 = 0.2;
pub const TITLE_MAX_CHARS: usize = 80;
pub const TITLE_WIRE_MAX_CHARS: usize = 120;
pub const TITLER_MAX_ABSTRACTS: usize = 10;
pub const TITLER_ABSTRACT_SLICE_CHARS: usize = 240;

// ── Link extraction (pipeline/session-metadata.ts) ───────────────────────────

pub const LINK_EXTRACT_SYSTEM_PROMPT: &str = "You extract code-collaboration references from one-sentence summaries of an AI-coding session. Report ONLY references that explicitly appear in the text: pull/merge request URLs, issue URLs, commit URLs, the `org/repo#123` shorthand, and ticket keys like ENG-123 or PROJ-4567. Output one reference per line, verbatim, nothing else — no prose, no numbering, no markdown. If the summaries contain no such reference, reply with exactly `none`. Never invent a reference, a repo, a number, or a URL that is not present in the text.";

pub const LINK_EXTRACT_MAX_TOKENS: u32 = 120;
pub const LINK_EXTRACT_TEMPERATURE: f64 = 0.1;
pub const LINK_EXTRACT_MAX_ABSTRACTS: usize = 12;

// ── Script summary (pipeline/script-summary.ts) ──────────────────────────────

pub const SCRIPT_SUMMARY_SYSTEM_PROMPT: &str = r#"You summarise what a script file does, for an engineer scanning a dashboard.

Rules:
- Output ONE plain sentence (at most 200 characters) stating what the script DOES when it runs.
- Be concrete and factual: the actions it performs and the systems it touches.
- No preamble ("This script…"), no markdown, no quotes, no line breaks.
- Do not invent behaviour that is not in the file. If the file is trivial or unreadable, say so briefly.
- Never include secrets, tokens, passwords, or personal data, even if they appear in the file."#;

pub const SCRIPT_SUMMARY_INPUT_MAX_CHARS: usize = 6000;
pub const SCRIPT_SUMMARY_OUTPUT_MAX_CHARS: usize = 200;
pub const SCRIPT_SUMMARY_TEMPERATURE: f64 = 0.2;

// ── Redaction backstop (pipeline/redaction.ts) ───────────────────────────────

pub const REDACT_SYSTEM_PROMPT: &str = "You are a security redaction reviewer. You are given a single shell command that has already been partly redacted. Find any remaining SECRETS or sensitive credentials still present in plaintext: API keys, access tokens, bearer tokens, passwords, private keys, connection strings with credentials, or other high-entropy secret values. Do NOT flag: program names, flags, file paths, hostnames, service/environment names (prod, dev), or existing [REDACTED:...] markers. Output ONLY the exact secret substrings, one per line, copied verbatim character-for-character as they appear in the command. If there are no remaining secrets, output exactly NONE. Output nothing else — no prose, no explanation, no numbering.";

pub const REDACT_MAX_TOKENS: u32 = 512;
pub const REDACT_TEMPERATURE: f64 = 0.0;

// ── Segmentation constants (pipeline/index.ts, feature §18) ───────────────────

pub const SEGMENT_TIME_GAP_MS: i64 = 15 * 60_000;
pub const SEGMENT_TOPIC_THRESHOLD: f64 = 0.35;
pub const SEGMENT_MAX_TURNS: usize = 100;
pub const SEGMENT_MAX_DURATION_MS: i64 = 30 * 60_000;
pub const SEGMENT_MAX_CONTENT_CHARS: usize = 12_000;
pub const SUMMARISER_EXCERPT_COUNT: usize = 7;
pub const SUMMARISER_EXCERPT_MAX_CHARS: usize = 350;

/// Per-excerpt slice inside the summariser prompt (index.ts `sampleAndRedactExcerpts`).
pub const EXCERPT_SAMPLE_MAX_CHARS: usize = 200;

/// User-intent distillation caps (index.ts `summariseUserIntent`) — kept separate
/// from the abstract so the abstract's contract is untouched. ≤6 user messages
/// (4 head + 2 tail) feed one ≤240-char, 120-token best-effort call.
pub const USER_INTENT_MAX_CHARS: usize = 240;
pub const USER_INTENT_MAX_TOKENS: u32 = 120;
pub const USER_INTENT_SAMPLE_HEAD: usize = 4;
pub const USER_INTENT_SAMPLE_TAIL: usize = 2;

// ── Pure user-prompt builders (cheap, so tests pin the exact prompt) ──────────

/// Collapse whitespace runs to a single space and trim (JS `.replace(/\s+/g," ").trim()`).
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The cognition user message (operates on the already-redacted abstract).
pub fn build_cognition_user_prompt(abstract_text: &str) -> String {
    format!(
        "Summary: \"{}\"\n\nOutput JSON only.",
        take_chars(&collapse(abstract_text), 480)
    )
}

/// The link-extraction user message over sampled abstracts.
pub fn build_link_extract_user_prompt(abstracts: &[String]) -> String {
    let lines = abstracts
        .iter()
        .enumerate()
        .map(|(i, a)| format!("  [part {}] {}", i + 1, take_chars(&collapse(a), 240)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Summaries of the session's parts:\n{lines}\n\nList the references, one per line, or \"none\"."
    )
}

/// The script-summary user message: the ref + capped contents.
pub fn build_script_summary_user_prompt(reference: &str, content: &str) -> String {
    let body = take_chars(content, SCRIPT_SUMMARY_INPUT_MAX_CHARS);
    [
        format!("Script reference: {reference}"),
        "Contents:".to_string(),
        "```".to_string(),
        body,
        "```".to_string(),
        String::new(),
        "One sentence (≤200 chars): what does running this script do?".to_string(),
    ]
    .join("\n")
}

/// The summariser user message — the per-slice facts line + sampled excerpts
/// (index.ts `summariseSlice`). `passes::summarize` supplies the frozen SYSTEM
/// prompt; this is the USER turn. `facts` is the `repo …; branch …; N turns on …`
/// line; `excerpts` are the already-redacted, ≤200-char samples (each whitespace-
/// collapsed into the `[turn i] "…"` block here). Empty facts → "generic coding
/// session" (TS `promptFacts || …`).
pub fn build_summariser_user_prompt(facts: &str, excerpts: &[String]) -> String {
    let excerpt_block = excerpts
        .iter()
        .enumerate()
        .map(|(i, e)| format!("  [turn {}] \"{}\"", i + 1, collapse(e)))
        .collect::<Vec<_>>()
        .join("\n");
    let facts = if facts.is_empty() {
        "generic coding session"
    } else {
        facts
    };
    format!(
        "Session context: {facts}.\n\nSampled excerpts from the conversation (already redacted of PII and secrets):\n{excerpt_block}\n\nWrite a ≤{ABSTRACT_OUTPUT_MAX_CHARS}-char summary (1-2 sentences) naming exactly what was achieved: the concrete action, what it acted on, and the specific target (repo/branch/service/component) when identifiable from the context above. Lead with an outcome verb and pack in concrete domain keywords (frameworks, features, decisions). Skip narration and filler."
    )
}

/// The user-intent user message — distills what the DEVELOPER asked for from
/// their OWN messages (index.ts `summariseUserIntent`). Each already-collapsed
/// message is sliced to [`USER_INTENT_MAX_CHARS`] in the `[msg i] "…"` block.
pub fn build_user_intent_user_prompt(messages: &[String]) -> String {
    let block = messages
        .iter()
        .enumerate()
        .map(|(i, e)| {
            format!(
                "  [msg {}] \"{}\"",
                i + 1,
                take_chars(e, USER_INTENT_MAX_CHARS)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The developer's own messages to an AI coding assistant (already redacted of PII and secrets):\n{block}\n\nIn ≤{USER_INTENT_MAX_CHARS} chars, summarise WHAT THE DEVELOPER ASKED FOR or DIRECTED — their goal or task in their own framing, AND any standing preferences / directives / conventions they expressed (e.g. \"always be thorough\", \"ship fast\", a naming or workflow convention). Focus on the DEVELOPER'S intent and voice, NOT what the assistant did. Reply with only the summary."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_carry_their_load_bearing_clauses() {
        assert!(SUMMARISER_SYSTEM_PROMPT.contains("Reply with only the summary."));
        assert!(SUMMARISER_SYSTEM_PROMPT.contains("≤ 400 characters"));
        assert!(COGNITION_SYSTEM_PROMPT.contains("\"emotions\":[],\"meta\":[],\"posture\":[]"));
        assert!(COGNITION_SYSTEM_PROMPT.contains("DO NOT emit ACTIVITY verbs"));
        assert!(TITLER_SYSTEM_PROMPT.contains("3-8 words"));
        assert!(TITLER_SYSTEM_PROMPT.contains(" · "));
        assert!(LINK_EXTRACT_SYSTEM_PROMPT.contains("org/repo#123"));
        assert!(SCRIPT_SUMMARY_SYSTEM_PROMPT.contains("at most 200 characters"));
        assert!(REDACT_SYSTEM_PROMPT.contains("output exactly NONE"));
    }

    #[test]
    fn cognition_builder_collapses_and_caps() {
        let p = build_cognition_user_prompt("  fixed   the\tbug  ");
        assert_eq!(p, "Summary: \"fixed the bug\"\n\nOutput JSON only.");
    }

    #[test]
    fn link_extract_builder_numbers_parts() {
        let p = build_link_extract_user_prompt(&["did a".into(), "did b".into()]);
        assert!(p.contains("[part 1] did a"));
        assert!(p.contains("[part 2] did b"));
        assert!(p.ends_with("or \"none\"."));
    }

    #[test]
    fn script_builder_caps_body() {
        let big = "x".repeat(SCRIPT_SUMMARY_INPUT_MAX_CHARS + 500);
        let p = build_script_summary_user_prompt("./deploy.sh", &big);
        assert!(p.contains("Script reference: ./deploy.sh"));
        // Body capped at the input max.
        let body_len = p.matches('x').count();
        assert_eq!(body_len, SCRIPT_SUMMARY_INPUT_MAX_CHARS);
    }
}
