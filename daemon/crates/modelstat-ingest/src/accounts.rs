//! Which provider account is logged in — `~/.modelstat/accounts.json`.
//!
//! Discovery already reads the logged-in account off disk on every heartbeat
//! (`modelstat_parsers::discovery::probe_identities`). This is where that
//! reading is REMEMBERED, so a scan can say which account produced a session
//! instead of leaving the server to infer it from the session's model providers.
//!
//! ## Why a timestamp, and why it is the whole point
//!
//! A transcript does not record the account that produced it — verified: Claude
//! Code's JSONL carries no account field of any kind. So the only source is
//! "who is logged in", which is a fact about **now**, not about the session.
//!
//! Naively stamping the current account onto every session is wrong in a way
//! that costs real money: on a fresh install the daemon scans MONTHS of old
//! transcripts in one pass, and every one of them would be labelled with today's
//! account. Same for anything that ran before an account switch.
//!
//! So each account carries `observed_since` — when we first saw THIS account for
//! THIS provider. An event older than that was produced under a login we cannot
//! name, and [`account_for_session`] declines rather than guessing. Attributing
//! money to the wrong account is worse than leaving it visibly unattributed.
//!
//! `observed_since` is reset ONLY when the account itself changes; re-seeing the
//! same account keeps it, so the window grows the longer you stay logged in and
//! steady-state scanning always stamps.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use modelstat_wire::schema::RawEvent;

use crate::paths::{ensure_home, home_path};

/// One provider's currently-logged-in account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSnapshot {
    /// The vendor's own account id (Anthropic's `oauthAccount.accountUuid`, the
    /// `sub` of Codex's JWT, …) — the id the server translates into ours.
    pub provider_account_id: String,
    /// Epoch ms when this account was FIRST seen for this provider. Events older
    /// than this predate our knowledge and are never attributed to it.
    pub observed_since: i64,
}

/// provider → the account logged in for it.
pub type Accounts = BTreeMap<String, AccountSnapshot>;

/// `~/.modelstat/accounts.json` (honors `MODELSTAT_HOME`; computed per call).
pub fn accounts_path() -> PathBuf {
    home_path("accounts.json")
}

/// Read the remembered accounts. A missing or unreadable file is an empty map,
/// never an error: not knowing the account is a supported state (it just means
/// nothing gets stamped), so a corrupt file must degrade to inference rather
/// than stop the daemon.
#[must_use]
pub fn load_accounts() -> Accounts {
    std::fs::read_to_string(accounts_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Persist the accounts map atomically (`0600`, like every other daemon file).
///
/// # Errors
/// Propagates the write/rename failure.
pub fn save_accounts(a: &Accounts) -> std::io::Result<()> {
    ensure_home()?;
    let path = accounts_path();
    let tmp = crate::atomic::with_pid_tmp(&path);
    let json = serde_json::to_string_pretty(a).expect("Accounts always serializes");
    std::fs::write(&tmp, json)?;
    crate::atomic::set_file_0600(&tmp);
    std::fs::rename(&tmp, &path)?;
    crate::atomic::set_file_0600(&path);
    Ok(())
}

/// Fold a fresh discovery reading into what we already remember.
///
/// `detected` is `(provider, provider_account_id)` — exactly what
/// `DetectedIdentity` carries. An unchanged account KEEPS its `observed_since`
/// (that window is the reason this file exists); a changed one starts a new
/// window at `now_ms`.
///
/// A provider that has dropped out of discovery is kept, not dropped: a probe
/// can fail transiently (a locked keychain, a slow `security` call), and
/// forgetting the account on a blip would silently stop attribution and reset
/// the window when it came back.
#[must_use]
pub fn fold_discovery(stored: &Accounts, detected: &[(String, String)], now_ms: i64) -> Accounts {
    let mut out = stored.clone();
    for (provider, account_id) in detected {
        if provider.is_empty() || account_id.is_empty() {
            continue; // half a key is not a key
        }
        match out.get(provider) {
            // Same account still logged in — keep the window it has earned.
            Some(existing) if existing.provider_account_id == *account_id => {}
            // New account, or a switch: the window starts now.
            _ => {
                out.insert(
                    provider.clone(),
                    AccountSnapshot {
                        provider_account_id: account_id.clone(),
                        observed_since: now_ms,
                    },
                );
            }
        }
    }
    out
}

/// The account to name for a session, or `None` when naming one would be a
/// guess.
///
/// `oldest_event_ms` is the oldest event being shipped for the session. It must
/// be at or after `observed_since` — otherwise part of this session ran under a
/// login we never saw, and the honest answer is to say nothing and let the
/// server surface it as needing an account.
#[must_use]
pub fn account_for_session<'a>(
    accounts: &'a Accounts,
    provider: &str,
    oldest_event_ms: i64,
) -> Option<&'a AccountSnapshot> {
    accounts
        .get(provider)
        .filter(|a| oldest_event_ms >= a.observed_since)
}

/// Build the batch's `session_installs`: for each session in `events`, the
/// account that produced it, when we can say so honestly.
///
/// One entry per session, keyed by session id, holding `(provider,
/// providerAccountId)` — the pair the server translates into an identity. A
/// session is OMITTED (never sent empty) whenever we cannot name its account:
///
/// * no probe for its provider (`zhipu`, `xai`, …) — nothing was ever read;
/// * any of its events predate [`AccountSnapshot::observed_since`] — the install
///   backlog and the pre-switch case, where today's login says nothing about who
///   ran it;
/// * its events disagree about the provider — a session that used two vendors
///   has no single account, and picking one would be a guess.
///
/// Returns `None` when no session could be named, so the field is left off the
/// wire entirely rather than riding as an empty map.
///
/// An event whose timestamp will not parse makes its session unnameable — we
/// cannot place it against the observation window, so we do not claim it.
#[must_use]
pub fn session_installs_for(events: &[RawEvent], accounts: &Accounts) -> Option<Value> {
    // session -> (the one provider it used, its oldest event) — or Poisoned once
    // the session turns out to span providers / carry an unreadable timestamp.
    enum Seen {
        One(String, i64),
        Poisoned,
    }
    let mut by_session: BTreeMap<String, Seen> = BTreeMap::new();
    for e in events {
        let (session_id, provider) = (e.session_id.clone(), e.provider.clone());
        if session_id.is_empty() {
            continue;
        }
        let ms = chrono::DateTime::parse_from_rfc3339(&e.ts)
            .ok()
            .map(|t| t.timestamp_millis());
        let entry = by_session
            .entry(session_id)
            .or_insert(Seen::One(provider.clone(), ms.unwrap_or(i64::MIN)));
        match entry {
            Seen::Poisoned => {}
            Seen::One(p, oldest) => match ms {
                // A second vendor in the same session: no single account owns it.
                _ if *p != provider => *entry = Seen::Poisoned,
                // An unreadable timestamp can't be placed against the window.
                None => *entry = Seen::Poisoned,
                Some(ms) => *oldest = (*oldest).min(ms),
            },
        }
    }

    let mut out = serde_json::Map::new();
    for (session_id, seen) in by_session {
        let Seen::One(provider, oldest) = seen else {
            continue;
        };
        let Some(account) = account_for_session(accounts, &provider, oldest) else {
            continue;
        };
        out.insert(
            session_id,
            // snake_case: the `/v1/ingest` contract is snake_case throughout
            // (`modelstat_core::contract::SessionInstall` declares no rename), so
            // a camelCase key here would deserialize to `None` and be silently
            // dropped — the field would look sent and attribute nothing.
            serde_json::json!({
                "provider": provider,
                "provider_account_id": account.provider_account_id,
            }),
        );
    }
    (!out.is_empty()).then(|| Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env_lock;

    fn with_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _g = test_env_lock();
        std::env::set_var("MODELSTAT_HOME", dir);
        let out = f();
        std::env::remove_var("MODELSTAT_HOME");
        out
    }

    fn acct(id: &str, since: i64) -> AccountSnapshot {
        AccountSnapshot {
            provider_account_id: id.into(),
            observed_since: since,
        }
    }

    #[test]
    fn first_sighting_starts_the_window_now() {
        let out = fold_discovery(
            &Accounts::new(),
            &[("anthropic".into(), "uuid-a".into())],
            1_000,
        );
        assert_eq!(out["anthropic"], acct("uuid-a", 1_000));
    }

    #[test]
    fn re_seeing_the_same_account_keeps_its_window() {
        let stored = Accounts::from([("anthropic".into(), acct("uuid-a", 1_000))]);
        let out = fold_discovery(&stored, &[("anthropic".into(), "uuid-a".into())], 9_999);
        assert_eq!(
            out["anthropic"].observed_since, 1_000,
            "the window is how long this login has been current — re-seeing it must not reset it"
        );
    }

    #[test]
    fn switching_account_restarts_the_window() {
        let stored = Accounts::from([("anthropic".into(), acct("uuid-a", 1_000))]);
        let out = fold_discovery(&stored, &[("anthropic".into(), "uuid-b".into())], 5_000);
        assert_eq!(out["anthropic"], acct("uuid-b", 5_000));
    }

    #[test]
    fn a_provider_missing_from_this_reading_is_kept_not_forgotten() {
        // A locked keychain / slow probe must not look like "logged out".
        let stored = Accounts::from([("anthropic".into(), acct("uuid-a", 1_000))]);
        let out = fold_discovery(&stored, &[("openai".into(), "uuid-o".into())], 5_000);
        assert_eq!(out["anthropic"], acct("uuid-a", 1_000));
        assert_eq!(out["openai"], acct("uuid-o", 5_000));
    }

    #[test]
    fn half_a_key_is_ignored() {
        let out = fold_discovery(
            &Accounts::new(),
            &[
                ("anthropic".into(), String::new()),
                (String::new(), "uuid-x".into()),
            ],
            1_000,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn events_from_before_we_saw_the_account_are_never_attributed_to_it() {
        // The install-day backlog: months of transcripts, account first seen now.
        let accounts = Accounts::from([("anthropic".into(), acct("uuid-a", 5_000))]);
        assert!(
            account_for_session(&accounts, "anthropic", 4_999).is_none(),
            "an event older than the account snapshot must not be stamped"
        );
        assert_eq!(
            account_for_session(&accounts, "anthropic", 5_000)
                .map(|a| a.provider_account_id.as_str()),
            Some("uuid-a"),
            "an event from exactly the observation instant onward is ours to name"
        );
    }

    #[test]
    fn a_provider_we_never_probed_names_nothing() {
        let accounts = Accounts::from([("anthropic".into(), acct("uuid-a", 0))]);
        // zhipu / xai have no probe — there is nothing to guess from.
        assert!(account_for_session(&accounts, "zhipu", 9_999).is_none());
    }

    /// A minimal event: only session / provider / ts matter here.
    fn ev(session: &str, provider: &str, ts: &str) -> RawEvent {
        RawEvent {
            source_event_id: format!("evt:{session}:{ts}"),
            ts: ts.into(),
            kind: "assistant_message".into(),
            agent: "claude_code".into(),
            provider: provider.into(),
            model: None,
            session_id: session.into(),
            turn_index: None,
            parent_event_id: None,
            cwd: None,
            git: None,
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

    fn installs(rows: &[(&str, &str, &str)], accounts: &Accounts) -> Option<Value> {
        let events: Vec<RawEvent> = rows.iter().map(|(s, p, ts)| ev(s, p, ts)).collect();
        session_installs_for(&events, accounts)
    }

    fn anthropic_since(since: i64) -> Accounts {
        Accounts::from([("anthropic".into(), acct("uuid-a", since))])
    }

    #[test]
    fn names_the_account_for_a_session_inside_the_window() {
        let got = installs(
            &[("sess_a", "anthropic", "1970-01-01T00:00:05.000Z")],
            &anthropic_since(1_000),
        );
        assert_eq!(
            got,
            Some(serde_json::json!({
                "sess_a": { "provider": "anthropic", "provider_account_id": "uuid-a" }
            }))
        );
    }

    #[test]
    fn the_wire_keys_are_snake_case() {
        // A camelCase key would deserialize to None server-side and be dropped
        // silently — the field would look sent and attribute nothing.
        let got = installs(
            &[("sess_a", "anthropic", "1970-01-01T00:00:05.000Z")],
            &anthropic_since(0),
        )
        .unwrap();
        let entry = &got["sess_a"];
        assert!(entry.get("provider_account_id").is_some(), "snake_case key");
        assert!(entry.get("providerAccountId").is_none(), "not camelCase");
    }

    #[test]
    fn the_oldest_event_decides_not_the_newest() {
        // A session straddling the switch: its later events are inside the
        // window, but part of it ran under a login we never saw.
        let got = installs(
            &[
                ("sess_a", "anthropic", "1970-01-01T00:00:00.900Z"),
                ("sess_a", "anthropic", "1970-01-01T00:00:05.000Z"),
            ],
            &anthropic_since(1_000),
        );
        assert_eq!(
            got, None,
            "one event before the window disqualifies the session"
        );
    }

    #[test]
    fn a_session_spanning_two_providers_is_left_alone() {
        let accounts = Accounts::from([
            ("anthropic".into(), acct("uuid-a", 0)),
            ("openai".into(), acct("uuid-o", 0)),
        ]);
        let got = installs(
            &[
                ("sess_a", "anthropic", "1970-01-01T00:00:05.000Z"),
                ("sess_a", "openai", "1970-01-01T00:00:05.001Z"),
            ],
            &accounts,
        );
        assert_eq!(
            got, None,
            "two vendors, no single owning account — say nothing"
        );
    }

    #[test]
    fn an_unreadable_timestamp_disqualifies_the_session() {
        let got = installs(
            &[("sess_a", "anthropic", "not-a-timestamp")],
            &anthropic_since(0),
        );
        assert_eq!(
            got, None,
            "cannot place it against the window → cannot name it"
        );
    }

    #[test]
    fn nameable_and_unnameable_sessions_in_one_batch() {
        let accounts = anthropic_since(1_000);
        let got = installs(
            &[
                ("sess_ok", "anthropic", "1970-01-01T00:00:05.000Z"), // in the window
                ("sess_old", "anthropic", "1970-01-01T00:00:00.500Z"), // backlog
                ("sess_zhipu", "zhipu", "1970-01-01T00:00:05.000Z"),  // no probe
            ],
            &accounts,
        )
        .unwrap();
        assert!(got.get("sess_ok").is_some());
        assert!(got.get("sess_old").is_none(), "backlog is never stamped");
        assert!(got.get("sess_zhipu").is_none(), "no probe → no account");
    }

    #[test]
    fn nothing_nameable_omits_the_field_entirely() {
        // An empty map would be a claim; absence is the honest shape.
        assert_eq!(
            installs(
                &[("sess_a", "zhipu", "1970-01-01T00:00:05.000Z")],
                &anthropic_since(0)
            ),
            None
        );
        assert_eq!(installs(&[], &anthropic_since(0)), None);
    }

    #[test]
    fn a_missing_or_corrupt_file_reads_as_empty_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            assert!(load_accounts().is_empty(), "missing file → empty");
            std::fs::write(accounts_path(), b"{ not json").unwrap();
            assert!(load_accounts().is_empty(), "corrupt file → empty, no panic");
        });
    }

    #[test]
    fn round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let a = Accounts::from([
                ("anthropic".into(), acct("uuid-a", 1_000)),
                ("openai".into(), acct("uuid-o", 2_000)),
            ]);
            save_accounts(&a).unwrap();
            assert_eq!(load_accounts(), a);
        });
    }
}
