//! The scoring seam: ask a model where this PR sits RELATIVE to the repo's own
//! human-authored anchors.
//!
//! Two things make this defensible rather than a vibe:
//!
//! 1. **The model is never asked for hours.** It is handed 5–8 of this repo's
//!    real human PRs with their measured `active_minutes` and asked only to
//!    place the target among them, on `0..1`. The minutes come from
//!    [`crate::calibrate`], from the anchors, not from the model. A model that
//!    is systematically optimistic about absolute durations cannot express that
//!    bias through this interface.
//! 2. **The crate never opens a socket.** [`Scorer`] is injected — the daemon
//!    passes a loopback or org self-hosted client, tests pass a closure. That
//!    is also why the whole path is testable with zero network.
//!
//! What crosses the seam is a prompt built from counts, extensions and
//! structure-only line shapes ([`crate::diff::structure_excerpt`]) — no
//! identifiers, no paths, no commit messages — so even a self-hosted engine
//! sees shape, not source.

use modelstat_wire::AnchorPr;
use serde::Deserialize;

use crate::diff::DiffFeatures;

/// Reference PRs shown to the model. Below five the baseline is too thin to
/// place anything against, and [`judge`] declines rather than inviting a guess;
/// above eight the prompt turns into a table nobody reads.
pub const MIN_REFERENCE_ANCHORS: usize = 5;
pub const MAX_REFERENCE_ANCHORS: usize = 8;

/// Longest category / risk-domain string kept from a reply.
const MAX_LABEL: usize = 32;
const MAX_RISK_DOMAINS: usize = 6;

/// Whatever can turn a prompt into a reply: a local llama.cpp session, an org
/// self-hosted endpoint, or a test closure. `None` for "no answer" — a judge
/// that is down degrades the estimate, it never fails it.
pub trait Scorer {
    fn score(&self, prompt: &str) -> Option<String>;
}

impl<F: Fn(&str) -> Option<String>> Scorer for F {
    fn score(&self, prompt: &str) -> Option<String> {
        self(prompt)
    }
}

/// What the model said, sanitized. Every float is finite and in `0..=1`.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgedFeatures {
    /// Coarse kind of change (`feature`, `bugfix`, `refactor`, …).
    pub category: String,
    pub novelty_0_1: f64,
    pub boilerplate_fraction_0_1: f64,
    pub risk_domains: Vec<String>,
    /// `0` = easier than every reference PR, `1` = harder than all of them.
    /// The single load-bearing number: [`crate::calibrate`] reads it as a
    /// quantile of the anchor distribution.
    pub relative_position_0_1: f64,
}

impl JudgedFeatures {
    /// How much the interval should widen for *this* judgement, `0..=1`.
    ///
    /// Novel work is where estimates go wrong, and boilerplate is where they go
    /// right — a 900-line CRUD scaffold is predictable in a way a new
    /// consensus path never is. So uncertainty rises with novelty and falls
    /// with boilerplate, equally weighted.
    pub fn uncertainty(&self) -> f64 {
        (0.5 * self.novelty_0_1 + 0.5 * (1.0 - self.boilerplate_fraction_0_1)).clamp(0.0, 1.0)
    }
}

/// The anchors worth showing the model: human-authored, with measured effort,
/// spread evenly across the repo's effort range rather than clustered. Pure.
///
/// Even spacing is the point. Eight anchors drawn from the fat middle would
/// give the model no idea what "hard for this team" looks like, and every
/// target would land near the median.
pub fn reference_anchors(anchors: &[AnchorPr]) -> Vec<&AnchorPr> {
    let mut usable: Vec<&AnchorPr> = anchors
        .iter()
        .filter(|a| !a.ai_assisted && a.active_minutes.is_some_and(|m| m > 0))
        .collect();
    usable.sort_by_key(|a| (a.active_minutes.unwrap_or(0), a.pr_number));
    let n = usable.len();
    if n <= MAX_REFERENCE_ANCHORS {
        return usable;
    }
    let k = MAX_REFERENCE_ANCHORS;
    (0..k)
        .map(|i| (i * (n - 1) + (k - 1) / 2) / (k - 1))
        .map(|idx| usable[idx.min(n - 1)])
        .collect()
}

/// The frozen prompt. Deterministic for a given diff + anchor set, so two runs
/// on the same commit ask the same question.
pub fn build_prompt(target: &DiffFeatures, anchors: &[AnchorPr]) -> String {
    let refs = reference_anchors(anchors);
    let mut p = String::with_capacity(2048 + target.excerpt.len());
    p.push_str(
        "You are sizing one merged pull request against pull requests from the SAME repository.\n\
         Do not estimate hours. Only place the TARGET relative to the REFERENCES below.\n\n\
         REFERENCES (human-authored merged PRs from this repo; active_minutes was measured from\n\
         their own commit timestamps, clustered into work sessions):\n",
    );
    for (i, a) in refs.iter().enumerate() {
        let m = a.active_minutes.unwrap_or(0);
        p.push_str(&format!(
            "  ref {}: files={} +{}/-{} commits={} active={}min\n",
            i + 1,
            a.files_changed,
            a.lines_added,
            a.lines_deleted,
            a.commit_count.unwrap_or(0),
            m,
        ));
    }
    let langs = target
        .languages
        .iter()
        .map(|(e, n)| format!("{e}:{n}"))
        .collect::<Vec<_>>()
        .join(",");
    p.push_str(&format!(
        "\nTARGET:\n  \
         files={} +{}/-{} hunks={} languages={}\n  \
         churn by kind: test={} config={} docs={} generated={} other={}\n",
        target.files_changed,
        target.lines_added,
        target.lines_deleted,
        target.hunks,
        if langs.is_empty() { "none" } else { langs.as_str() },
        target.test_lines,
        target.config_lines,
        target.doc_lines,
        target.generated_lines,
        target
            .churn()
            .saturating_sub(target.test_lines)
            .saturating_sub(target.config_lines)
            .saturating_sub(target.doc_lines)
            .saturating_sub(target.generated_lines),
    ));
    p.push_str(
        "\nSTRUCTURE (shapes only — `file N <class> .<ext>`, hunk headers, and one\n\
         `<sign><indent>/<length><b|c|x>` per changed line; no source text is included):\n",
    );
    p.push_str(&target.excerpt);
    p.push_str(
        "\nReply with ONE JSON object and nothing else:\n\
         {\"category\":\"feature|bugfix|refactor|test|docs|config|chore\",\
         \"novelty_0_1\":0.0,\
         \"boilerplate_fraction_0_1\":0.0,\
         \"risk_domains\":[\"auth\",\"data-migration\"],\
         \"relative_position_0_1\":0.0}\n\
         relative_position_0_1: 0.0 = less work than every reference, 1.0 = more than all of them.\n",
    );
    p
}

/// Reply shape. Only `risk_domains` is optional: novelty and boilerplate drive
/// the interval width, so silently defaulting them would fabricate a confidence
/// the model never expressed.
#[derive(Deserialize)]
struct RawReply {
    category: String,
    novelty_0_1: f64,
    boilerplate_fraction_0_1: f64,
    #[serde(default)]
    risk_domains: Vec<String>,
    relative_position_0_1: f64,
}

fn label(s: &str) -> Option<String> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    Some(t.chars().take(MAX_LABEL).collect())
}

/// Parse a model reply into [`JudgedFeatures`]. Pure, total: any malformed,
/// fenced, truncated or hostile reply is `None`, never a panic.
pub fn parse_reply(reply: &str) -> Option<JudgedFeatures> {
    // Models fence, preamble, and apologize. Take the outermost object.
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    if end <= start {
        return None;
    }
    let raw: RawReply = serde_json::from_str(&reply[start..=end]).ok()?;
    let unit = |v: f64| -> Option<f64> { v.is_finite().then(|| v.clamp(0.0, 1.0)) };
    Some(JudgedFeatures {
        category: label(&raw.category)?,
        novelty_0_1: unit(raw.novelty_0_1)?,
        boilerplate_fraction_0_1: unit(raw.boilerplate_fraction_0_1)?,
        risk_domains: raw
            .risk_domains
            .iter()
            .filter_map(|d| label(d))
            .take(MAX_RISK_DOMAINS)
            .collect(),
        relative_position_0_1: unit(raw.relative_position_0_1)?,
    })
}

/// Build the prompt, ask the scorer, parse the reply. `None` when the repo has
/// too few human anchors to place anything against (in which case the scorer is
/// never even called), when the scorer declines, or when the reply is junk.
pub fn judge(
    scorer: &dyn Scorer,
    target: &DiffFeatures,
    anchors: &[AnchorPr],
) -> Option<JudgedFeatures> {
    if reference_anchors(anchors).len() < MIN_REFERENCE_ANCHORS {
        return None;
    }
    parse_reply(&scorer.score(&build_prompt(target, anchors))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::anchor;

    #[test]
    fn parses_a_good_reply() {
        let j = parse_reply(
            r#"{"category":"Feature","novelty_0_1":0.7,"boilerplate_fraction_0_1":0.1,
                "risk_domains":["Auth","data-migration"],"relative_position_0_1":0.82}"#,
        )
        .expect("good reply");
        assert_eq!(j.category, "feature");
        assert_eq!(j.risk_domains, vec!["auth", "data-migration"]);
        assert!((j.relative_position_0_1 - 0.82).abs() < 1e-9);
        assert!((j.uncertainty() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn parses_a_fenced_reply_with_preamble() {
        let j = parse_reply(
            "Sure! Here is the JSON:\n```json\n\
             {\"category\":\"refactor\",\"novelty_0_1\":0.2,\
             \"boilerplate_fraction_0_1\":0.6,\"relative_position_0_1\":0.3}\n\
             ```\nHope that helps.",
        )
        .expect("fenced reply");
        assert_eq!(j.category, "refactor");
        assert!(j.risk_domains.is_empty());
    }

    #[test]
    fn clamps_out_of_range_numbers() {
        let j = parse_reply(
            r#"{"category":"chore","novelty_0_1":-4,"boilerplate_fraction_0_1":9,
                "relative_position_0_1":17}"#,
        )
        .expect("clamped reply");
        assert_eq!(j.novelty_0_1, 0.0);
        assert_eq!(j.boilerplate_fraction_0_1, 1.0);
        assert_eq!(j.relative_position_0_1, 1.0);
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "no json here",
            "{",
            "}{",
            r#"{"category":"feature"}"#,
            r#"{"category":"","novelty_0_1":0.1,"boilerplate_fraction_0_1":0.1,"relative_position_0_1":0.1}"#,
            r#"{"category":"feature","novelty_0_1":"high","boilerplate_fraction_0_1":0.1,"relative_position_0_1":0.1}"#,
            r#"{"novelty_0_1":0.1,"boilerplate_fraction_0_1":0.1,"relative_position_0_1":0.1}"#,
        ] {
            assert!(parse_reply(bad).is_none(), "accepted garbage: {bad:?}");
        }
    }

    #[test]
    fn reference_anchors_spread_across_the_range_and_skip_ai_prs() {
        let mut set: Vec<AnchorPr> = (1..=20).map(|i| anchor(i, 10, 100, Some(i as u32 * 10))).collect();
        set.push(AnchorPr {
            ai_assisted: true,
            ..anchor(99, 10, 100, Some(45))
        });
        set.push(anchor(98, 10, 100, None));
        let refs = reference_anchors(&set);
        assert_eq!(refs.len(), MAX_REFERENCE_ANCHORS);
        assert!(refs.iter().all(|a| !a.ai_assisted && a.active_minutes.is_some()));
        // Endpoints of the human distribution are always shown.
        assert_eq!(refs[0].active_minutes, Some(10));
        assert_eq!(refs[refs.len() - 1].active_minutes, Some(200));
        let mins: Vec<u32> = refs.iter().map(|a| a.active_minutes.unwrap()).collect();
        assert!(mins.windows(2).all(|w| w[0] < w[1]), "{mins:?}");
    }

    #[test]
    fn judge_declines_on_a_thin_baseline_without_calling_the_scorer() {
        let called = std::cell::Cell::new(false);
        let scorer = |_: &str| -> Option<String> {
            called.set(true);
            Some(r#"{"category":"feature","novelty_0_1":0.5,"boilerplate_fraction_0_1":0.5,"relative_position_0_1":0.5}"#.into())
        };
        let thin: Vec<AnchorPr> = (1..=4).map(|i| anchor(i, 5, 50, Some(60))).collect();
        assert!(judge(&scorer, &DiffFeatures::default(), &thin).is_none());
        assert!(!called.get(), "a thin baseline must not reach the model");
    }

    #[test]
    fn prompt_shows_references_and_never_source_text() {
        let anchors: Vec<AnchorPr> = (1..=6).map(|i| anchor(i, 4, 40, Some(i as u32 * 30))).collect();
        let target = crate::diff::features_from(
            "10\t2\tsrc/secret/token_store.rs",
            "diff --git a/src/secret/token_store.rs b/src/secret/token_store.rs\n\
             @@ -1,2 +1,3 @@ impl TokenStore {\n\
             +    let api_key = \"sk-live-DEADBEEF\";\n",
        );
        let p = build_prompt(&target, &anchors);
        assert!(p.contains("ref 1: files=4 +40/-40"), "{p}");
        assert!(p.contains("active=30min"));
        assert!(p.contains("languages=rs:1"));
        for leak in ["token_store", "sk-live", "api_key", "TokenStore"] {
            assert!(!p.contains(leak), "prompt leaked {leak:?}:\n{p}");
        }
        assert!(p.contains("+4/33x"), "shapes survive:\n{p}");
    }
}
