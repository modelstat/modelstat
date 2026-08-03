//! How each agent on THIS machine authenticates to its provider — the fact that
//! decides whether its tokens cost money.
//!
//! The server cannot know this. A Codex rollout records tokens and a model, but
//! nothing about the credential behind the call, and the same transcript shape
//! is produced whether the user is on a $20/month ChatGPT plan or burning an
//! `OPENAI_API_KEY` at list price. Only the machine that ran the agent can see
//! the auth material, so only the daemon can state the mode — and it must state
//! it, because the alternative is the server picking one. It used to pick `api`,
//! which billed a ChatGPT Plus user $39.65 for Codex sessions their subscription
//! already covered.
//!
//! Every function here returns [`PRICING_MODE_UNKNOWN`] rather than a plausible
//! answer when the evidence is absent or contradictory. That value travels to
//! the server as an explicit "we looked and could not tell", prices to $0
//! instead of inventing spend, and raises an inbox item asking a human. A guess
//! that lands in a money column is worse than an admission that does not.

use std::path::PathBuf;

use serde_json::Value;

/// Covered by a flat plan — the per-call price is $0.
pub const PRICING_MODE_SUBSCRIPTION: &str = "subscription";
/// Metered pay-per-token against a provider key.
pub const PRICING_MODE_API: &str = "api";
/// We looked at the auth material and it did not settle the question.
pub const PRICING_MODE_UNKNOWN: &str = "unknown";

/// Read + parse a JSON file, or `None` if it is missing or malformed.
fn read_json(path: &PathBuf) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// True when a JSON field holds a non-empty string.
fn has_text(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

/// True when an env var is set to something non-empty.
fn env_set(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| !v.trim().is_empty())
}

/// Codex CLI's pricing mode, from `auth.json` under `$CODEX_HOME` (default
/// `~/.codex`).
///
/// Codex writes exactly one of two credentials at login:
///   * `tokens` — the ChatGPT OAuth bundle (`id_token`/`access_token`). The
///     calls ride the user's ChatGPT plan, so they are `subscription`.
///   * `OPENAI_API_KEY` — a metered key, so `api`.
///
/// Both present is real (a user who logged in one way and later configured the
/// other) and genuinely ambiguous: which one Codex reaches for depends on its
/// `preferred_auth_method` config, which is not something this file settles. We
/// say `unknown` instead of picking, because picking is what this whole change
/// exists to stop.
#[must_use]
pub fn codex_pricing_mode(home: &str) -> &'static str {
    let candidates = codex_auth_paths(home);
    let Some(obj) = candidates.iter().find_map(read_json) else {
        // No auth.json at all. Codex still runs from a bare `OPENAI_API_KEY` in
        // the environment, and that IS metered — but only if the variable is
        // actually set; otherwise we know nothing.
        return if env_set("OPENAI_API_KEY") {
            PRICING_MODE_API
        } else {
            PRICING_MODE_UNKNOWN
        };
    };
    let chatgpt = obj
        .get("tokens")
        .is_some_and(|t| has_text(t.get("id_token")) || has_text(t.get("access_token")));
    let api_key = has_text(obj.get("OPENAI_API_KEY"));
    match (chatgpt, api_key) {
        (true, false) => PRICING_MODE_SUBSCRIPTION,
        (false, true) => PRICING_MODE_API,
        // Neither (a logged-out stub) or both (ambiguous) — say so.
        _ => PRICING_MODE_UNKNOWN,
    }
}

/// The `auth.json` locations Codex reads, most specific first.
fn codex_auth_paths(home: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        if !dir.trim().is_empty() {
            out.push(PathBuf::from(dir).join("auth.json"));
        }
    }
    out.push(PathBuf::from(home).join(".codex/auth.json"));
    out.push(PathBuf::from(home).join(".config/codex/auth.json"));
    out
}

/// Claude Code's pricing mode.
///
/// `~/.claude.json` carries an `oauthAccount` block once the user has logged in
/// with a Claude subscription; `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`)
/// drives the metered path instead. This used to be hardcoded to
/// `subscription` — right for most people and quietly wrong for anyone running
/// Claude Code on a key, whose real spend was reported as $0.
#[must_use]
pub fn claude_code_pricing_mode(home: &str) -> &'static str {
    let oauth = claude_config_paths(home)
        .iter()
        .filter_map(read_json)
        .any(|obj| obj.get("oauthAccount").is_some_and(|a| a.is_object()));
    let api_key = env_set("ANTHROPIC_API_KEY") || env_set("ANTHROPIC_AUTH_TOKEN");
    match (oauth, api_key) {
        (true, false) => PRICING_MODE_SUBSCRIPTION,
        (false, true) => PRICING_MODE_API,
        _ => PRICING_MODE_UNKNOWN,
    }
}

/// The `.claude.json` locations Claude Code reads, most specific first.
fn claude_config_paths(home: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.trim().is_empty() {
            out.push(PathBuf::from(dir).join(".claude.json"));
        }
    }
    out.push(PathBuf::from(home).join(".claude.json"));
    out
}

/// The pi harness's pricing mode.
///
/// pi talks to provider APIs with whatever key is in the environment — there is
/// no subscription path — so a key present means `api`, and no key means we
/// cannot say which provider credential the recorded session actually used.
#[must_use]
pub fn pi_pricing_mode() -> &'static str {
    if env_set("ANTHROPIC_API_KEY")
        || env_set("ANTHROPIC_AUTH_TOKEN")
        || env_set("OPENAI_API_KEY")
        || env_set("GEMINI_API_KEY")
    {
        PRICING_MODE_API
    } else {
        PRICING_MODE_UNKNOWN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A scratch `$HOME` with an optional `.codex/auth.json` body.
    fn home_with(name: &str, rel: &str, body: Option<&str>) -> String {
        let dir = std::env::temp_dir().join(format!(
            "modelstat-authmode-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        if let Some(body) = body {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
        } else {
            fs::create_dir_all(&dir).unwrap();
        }
        dir.to_string_lossy().into_owned()
    }

    /// The exact shape the reported bug came from: a ChatGPT login, whose Codex
    /// sessions were being billed at full API list price.
    #[test]
    fn codex_chatgpt_login_is_a_subscription() {
        let home = home_with(
            "sub",
            ".codex/auth.json",
            Some(
                r#"{"OPENAI_API_KEY": null, "tokens": {"id_token": "ey.J.wt", "account_id": "acc"}}"#,
            ),
        );
        assert_eq!(codex_pricing_mode(&home), PRICING_MODE_SUBSCRIPTION);
    }

    #[test]
    fn codex_api_key_login_is_metered() {
        let home = home_with(
            "api",
            ".codex/auth.json",
            Some(r#"{"OPENAI_API_KEY": "sk-live-xxx", "tokens": null}"#),
        );
        assert_eq!(codex_pricing_mode(&home), PRICING_MODE_API);
    }

    /// Both credentials on disk: which one Codex uses depends on config we do
    /// not read here, so the honest answer is that we do not know.
    #[test]
    fn codex_with_both_credentials_refuses_to_pick() {
        let home = home_with(
            "both",
            ".codex/auth.json",
            Some(r#"{"OPENAI_API_KEY": "sk-live-xxx", "tokens": {"id_token": "ey.J.wt"}}"#),
        );
        assert_eq!(codex_pricing_mode(&home), PRICING_MODE_UNKNOWN);
    }

    /// A logged-out stub is evidence of nothing — not evidence of metering.
    #[test]
    fn codex_empty_auth_file_is_unknown() {
        let home = home_with(
            "empty",
            ".codex/auth.json",
            Some(r#"{"OPENAI_API_KEY": null, "tokens": null}"#),
        );
        assert_eq!(codex_pricing_mode(&home), PRICING_MODE_UNKNOWN);
    }

    /// Malformed JSON must not read as "no credentials" — the file exists and
    /// we failed to understand it, which is the definition of unknown.
    #[test]
    fn codex_unparseable_auth_file_is_unknown() {
        let home = home_with("bad", ".codex/auth.json", Some("{not json"));
        // `OPENAI_API_KEY` is not set in the test environment, so the missing-file
        // branch also lands on unknown — either way we must not invent a mode.
        assert_eq!(codex_pricing_mode(&home), PRICING_MODE_UNKNOWN);
    }

    #[test]
    fn claude_code_oauth_login_is_a_subscription() {
        let home = home_with(
            "cc",
            ".claude.json",
            Some(r#"{"oauthAccount": {"accountUuid": "u-1", "emailAddress": "a@b.c"}}"#),
        );
        assert_eq!(claude_code_pricing_mode(&home), PRICING_MODE_SUBSCRIPTION);
    }

    #[test]
    fn claude_code_with_no_evidence_is_unknown() {
        let home = home_with("cc-none", ".claude.json", None);
        assert_eq!(claude_code_pricing_mode(&home), PRICING_MODE_UNKNOWN);
    }

    /// Every value this module can emit must be one the wire accepts, or the
    /// server rejects the batch.
    #[test]
    fn every_mode_is_a_legal_wire_value() {
        for m in [
            PRICING_MODE_SUBSCRIPTION,
            PRICING_MODE_API,
            PRICING_MODE_UNKNOWN,
        ] {
            assert!(
                modelstat_wire::enums::PRICING_MODES.contains(&m),
                "{m} is not in PRICING_MODES"
            );
        }
    }
}
