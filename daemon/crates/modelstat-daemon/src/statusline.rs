//! `modelstat statusline` — Feature 1's always-on surface (§14). A port of
//! `apps/daemon/src/statusline.ts`.
//!
//! Claude Code runs this on every prompt render, piping a JSON status object to
//! stdin; we print ONE compact line: a dim `modelstat` marker, the live token
//! count (from Claude Code's own context window — instant, no modelstat
//! dependency), then the effective $ assigned + taxonomy chips for the session
//! read from the LOCAL insights cache the daemon refreshes after a scan.
//!
//! HARD CONSTRAINTS (a statusline that blocks or throws wedges the prompt): never
//! await the network (the cache read is sync + tiny), never throw (every parse is
//! defensive; a failure degrades to the bare `modelstat` marker). A genuine cache
//! read/parse error is surfaced in `status`, never silently blanked (§21.12) —
//! but the render itself always produces a line.

use std::io::{Read, Write};

use serde::Deserialize;
use serde_json::Value;

use modelstat_ingest::home_path;

use crate::insights::{
    read_cached_insights_sync, SessionInsights, STATUS_NOT_INGESTED, STATUS_READY,
};

// ── ANSI (tiny + self-contained; no dependency) ──────────────────────────
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";

/// The subset of Claude Code's statusLine stdin payload we read — everything
/// optional (fields come and go by session state), so a field rename upstream
/// degrades gracefully instead of crashing.
#[derive(Debug, Default, Deserialize)]
pub struct StatuslineInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub context_window: Option<ContextWindow>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ContextWindow {
    #[serde(default)]
    pub total_input_tokens: Option<f64>,
    #[serde(default)]
    pub total_output_tokens: Option<f64>,
    #[serde(default)]
    pub current_usage: Option<CurrentUsage>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CurrentUsage {
    #[serde(default)]
    pub input_tokens: Option<f64>,
    #[serde(default)]
    pub output_tokens: Option<f64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<f64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<f64>,
}

/// Compact a token count: `1234` → `1.2k`, `1_200_000` → `1.2M` (§14 rules).
pub fn format_tokens(n: f64) -> String {
    if !n.is_finite() || n <= 0.0 {
        return "0".to_string();
    }
    if n < 1000.0 {
        return format!("{}", n.round() as i64);
    }
    if n < 1_000_000.0 {
        // Under 10k keeps one decimal (1.2k); above rounds to a whole k (16k).
        let dp: usize = if n < 10_000.0 { 1 } else { 0 };
        return format!("{:.*}k", dp, n / 1000.0);
    }
    format!("{:.1}M", n / 1_000_000.0)
}

/// Format an effective-cost value (the server sends an exact decimal string;
/// tolerate a number too). `None` when there's nothing meaningful (≤0 / NaN /
/// null / unparseable). Sub-cent spend keeps 4dp (it still matters when
/// attributing); a cent or more, 2dp.
pub fn format_cost(cost: Option<&Value>) -> Option<String> {
    let n = match cost? {
        Value::Number(num) => num.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    Some(if n < 0.01 {
        format!("${n:.4}")
    } else {
        format!("${n:.2}")
    })
}

/// Total context-window tokens for this turn — prefer Claude Code's explicit
/// totals, fall back to summing the current usage.
fn context_tokens(cw: Option<&ContextWindow>) -> f64 {
    let Some(cw) = cw else {
        return 0.0;
    };
    let totals = cw.total_input_tokens.unwrap_or(0.0) + cw.total_output_tokens.unwrap_or(0.0);
    if totals > 0.0 {
        return totals;
    }
    let Some(u) = &cw.current_usage else {
        return 0.0;
    };
    u.input_tokens.unwrap_or(0.0)
        + u.output_tokens.unwrap_or(0.0)
        + u.cache_creation_input_tokens.unwrap_or(0.0)
        + u.cache_read_input_tokens.unwrap_or(0.0)
}

/// Up to `max` taxonomy chips (`emoji name`), comma-joined, with a `+N` overflow.
fn render_taxonomy(insights: &SessionInsights, max: usize) -> String {
    let nodes = insights.taxonomy_nodes.as_deref().unwrap_or(&[]);
    if nodes.is_empty() {
        return String::new();
    }
    let chips: Vec<String> = nodes
        .iter()
        .take(max)
        .map(|n| {
            let emoji = n
                .emoji
                .as_deref()
                .map(|e| format!("{e} "))
                .unwrap_or_default();
            format!("{emoji}{}", n.name)
        })
        .collect();
    let extra = if nodes.len() > max {
        format!(" +{}", nodes.len() - max)
    } else {
        String::new()
    };
    format!("{}{extra}", chips.join(", "))
}

/// Build the one-line statusline (no trailing newline). Pure. Shape:
/// `modelstat <tokens> tok · <$> · <chips>`, missing pieces omitted; while the
/// server is still enriching, `analyzing…` stands in for the $ / chips.
pub fn render_statusline(input: &StatuslineInput, insights: Option<&SessionInsights>) -> String {
    let sep = format!("{DIM} · {RESET}");
    let mut parts: Vec<String> = Vec::new();

    // 1. Tokens — instant, from Claude Code's context window.
    let tok = context_tokens(input.context_window.as_ref());
    if tok > 0.0 {
        parts.push(format!(
            "{CYAN}{}{RESET}{DIM} tok{RESET}",
            format_tokens(tok)
        ));
    }

    // 2. modelstat layer — effective $ + taxonomy from the local cache.
    match insights {
        Some(ins) if ins.status != STATUS_NOT_INGESTED => {
            if let Some(cost) = format_cost(ins.cost_usd.as_ref()) {
                parts.push(format!("{GREEN}{cost}{RESET}"));
            }
            let tax = render_taxonomy(ins, 3);
            if !tax.is_empty() {
                parts.push(format!("{DIM}{tax}{RESET}"));
            } else if ins.status != STATUS_READY {
                // Not "is it exactly `analyzing`" — anything that is not the
                // explicit finished marker still has work coming. Testing for
                // `analyzing` meant a status this build has never heard of
                // rendered a blank line where "analyzing…" belongs, and the
                // server could not fix it without a fleet release.
                parts.push(format!("{DIM}analyzing…{RESET}"));
            }
        }
        // A `not_ingested` (or absent) cache contributes nothing beyond tokens.
        _ => {}
    }

    let body = parts.join(&sep);
    if body.is_empty() {
        format!("{DIM}modelstat{RESET}")
    } else {
        format!("{DIM}modelstat{RESET} {body}")
    }
}

/// Read up to 1 MB of stdin (far more than any status payload) — a misbehaving
/// caller can't make us buffer unbounded. Lossy UTF-8: garbage in still renders.
fn read_stdin_capped() -> String {
    let mut buf = Vec::new();
    let _ = std::io::stdin().take(1024 * 1024).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// CLI entry — `modelstat statusline`. Reads stdin, looks up the local insights
/// cache for the reported session, prints the one-liner. Never blocks on the
/// network and never throws: any failure degrades to the bare `modelstat` marker.
pub fn run_statusline() {
    let raw = read_stdin_capped();
    let input: StatuslineInput = if raw.trim().is_empty() {
        StatuslineInput::default()
    } else {
        serde_json::from_str(&raw).unwrap_or_default()
    };
    let insights = input
        .session_id
        .as_deref()
        .and_then(|sid| read_cached_insights_sync(&home_path("sessions"), sid));
    print!("{}", render_statusline(&input, insights.as_ref()));
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Strip SGR (`\x1b[…m`) so tests read against the visible text.
    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c2 in chars.by_ref() {
                    if c2 == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn cw_input() -> StatuslineInput {
        serde_json::from_value(json!({
            "session_id": "s1",
            "context_window": { "total_input_tokens": 12000, "total_output_tokens": 300 }
        }))
        .unwrap()
    }

    fn insights(v: Value) -> SessionInsights {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn format_tokens_compacts_to_k_and_m() {
        assert_eq!(format_tokens(0.0), "0");
        assert_eq!(format_tokens(950.0), "950");
        assert_eq!(format_tokens(1234.0), "1.2k");
        assert_eq!(format_tokens(15500.0), "16k");
        assert_eq!(format_tokens(2_300_000.0), "2.3M");
    }

    #[test]
    fn format_cost_subcent_4dp_else_2dp_junk_none() {
        assert_eq!(format_cost(Some(&json!("0.42"))).as_deref(), Some("$0.42"));
        assert_eq!(
            format_cost(Some(&json!(0.0031))).as_deref(),
            Some("$0.0031")
        );
        assert_eq!(format_cost(Some(&json!("0"))), None);
        assert_eq!(format_cost(Some(&json!(null))), None);
        assert_eq!(format_cost(None), None);
        assert_eq!(format_cost(Some(&json!("not-a-number"))), None);
    }

    #[test]
    fn tokens_render_from_context_window_without_insights() {
        let line = strip(&render_statusline(&cw_input(), None));
        assert!(line.contains("modelstat"), "{line}");
        assert!(line.contains("12k tok"), "{line}");
    }

    #[test]
    fn ready_insights_add_cost_and_taxonomy_chips() {
        let ins = insights(json!({
            "status": "ready",
            "cost_usd": "0.42",
            "taxonomy_nodes": [
                { "id": "n1", "name": "debugging", "emoji": "🐛" },
                { "id": "n2", "name": "auth" }
            ]
        }));
        let line = strip(&render_statusline(&cw_input(), Some(&ins)));
        assert!(line.contains("12k tok"), "{line}");
        assert!(line.contains("$0.42"), "{line}");
        assert!(line.contains("🐛 debugging"), "{line}");
        assert!(line.contains("auth"), "{line}");
    }

    #[test]
    fn more_than_three_nodes_collapse_to_a_plus_n_suffix() {
        let ins = insights(json!({
            "status": "ready",
            "taxonomy_nodes": [
                { "id": "1", "name": "a" }, { "id": "2", "name": "b" },
                { "id": "3", "name": "c" }, { "id": "4", "name": "d" },
                { "id": "5", "name": "e" }
            ]
        }));
        let line = strip(&render_statusline(&cw_input(), Some(&ins)));
        assert!(line.contains("a, b, c +2"), "{line}");
    }

    #[test]
    fn analyzing_shows_placeholder_no_cost() {
        let ins = insights(json!({ "status": "analyzing" }));
        let line = strip(&render_statusline(&cw_input(), Some(&ins)));
        assert!(line.contains("12k tok"), "{line}");
        assert!(line.contains("analyzing…"), "{line}");
        assert!(!line.contains('$'), "{line}");
    }

    #[test]
    fn an_unknown_status_reads_as_still_working() {
        // The placeholder is gated on "not finished", not on the one word this
        // build happens to know: a server that starts saying `queued` must not
        // silently blank the line on every installed daemon.
        for status in ["queued", "summarising", "retrying"] {
            let ins = insights(json!({ "status": status }));
            let line = strip(&render_statusline(&cw_input(), Some(&ins)));
            assert!(line.contains("analyzing…"), "{status}: {line}");
        }
        // …and `ready` with nothing to show still says nothing.
        let ready = insights(json!({ "status": "ready", "taxonomy_nodes": [] }));
        let line = strip(&render_statusline(&cw_input(), Some(&ready)));
        assert!(!line.contains("analyzing"), "{line}");
    }

    #[test]
    fn not_ingested_contributes_nothing_beyond_tokens() {
        let ins =
            insights(json!({ "status": "not_ingested", "cost_usd": "0", "taxonomy_nodes": [] }));
        let line = strip(&render_statusline(&cw_input(), Some(&ins)));
        assert!(line.contains("12k tok"), "{line}");
        assert!(!line.contains("analyzing"), "{line}");
    }

    #[test]
    fn empty_context_window_still_renders_the_marker() {
        let input: StatuslineInput = serde_json::from_value(json!({ "session_id": "s" })).unwrap();
        let line = strip(&render_statusline(&input, None));
        assert_eq!(line.trim(), "modelstat");
    }

    #[test]
    fn falls_back_to_summing_current_usage() {
        let input: StatuslineInput = serde_json::from_value(json!({
            "session_id": "s",
            "context_window": { "current_usage": {
                "input_tokens": 1000, "output_tokens": 200,
                "cache_read_input_tokens": 300, "cache_creation_input_tokens": 0
            }}
        }))
        .unwrap();
        let line = strip(&render_statusline(&input, None));
        assert!(line.contains("1.5k tok"), "{line}");
    }
}
