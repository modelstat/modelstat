//! `modelstat label <pr-number> <minutes>` — the only input to this whole
//! machine that is measured rather than inferred.
//!
//! [`modelstat_effort`] can score every PR in a repo without help, but the score
//! is dimensionless. Hours exist only once someone has written down, by hand,
//! how long at least [`MIN_LABELS`] PRs actually took. This command is where
//! that happens, and its whole job is to be cheap enough to run right after a
//! merge and strict enough that a typo cannot poison the fit.
//!
//! Local only: labels live beside `anchors.json` in the daemon home and are
//! never uploaded.

use std::collections::BTreeMap;
use std::process::ExitCode;

use modelstat_effort::{LabelStore, MIN_LABELS};
use modelstat_parsers::git::resolve_repo_root;
use modelstat_parsers::{mine_repo_anchors, AnchorConfig};

use crate::cmd_roi::{build_calibration, flag_value, labels_path, merged_prs};

/// A PR nobody could have spent longer than this on in one stretch — 7 days of
/// wall clock, in minutes. Past it the input is a units mix-up (someone typed
/// seconds, or hours-as-minutes), and one absurd label drags a two-parameter
/// log-space fit further than the other seven combined.
const MAX_MINUTES: u32 = 7 * 24 * 60;

/// What a label command is asking for, once the words are numbers.
#[derive(Debug)]
struct LabelArgs {
    pr_number: u64,
    minutes: u32,
    repo: String,
}

/// Parse + validate. Every rejection names the value it rejected, because the
/// user is at a terminal about to retype it.
fn parse_label_args(args: &[String]) -> Result<LabelArgs, String> {
    let repo = flag_value(args, "--repo").unwrap_or_else(|| ".".to_string());
    // Positionals only: everything that is not a flag or a flag's value.
    let mut positional: Vec<&String> = Vec::new();
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a == "--repo" {
            skip_next = true;
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        positional.push(a);
    }
    let (Some(pr_raw), Some(min_raw)) = (positional.first(), positional.get(1)) else {
        return Err(usage());
    };

    let pr_number: u64 = pr_raw
        .trim_start_matches('#')
        .parse()
        .map_err(|_| format!("modelstat label: `{pr_raw}` is not a PR number"))?;
    if pr_number == 0 {
        return Err("modelstat label: PR numbers start at 1".to_string());
    }

    let minutes: u32 = min_raw
        .parse()
        .map_err(|_| format!("modelstat label: `{min_raw}` is not a whole number of minutes"))?;
    if minutes == 0 {
        return Err(
            "modelstat label: 0 minutes is not an effort measurement — label the time it \
             actually took, in minutes"
                .to_string(),
        );
    }
    if minutes > MAX_MINUTES {
        return Err(format!(
            "modelstat label: {minutes} minutes is {:.0} days — did you mean minutes? \
             (the ceiling is {MAX_MINUTES})",
            f64::from(minutes) / (24.0 * 60.0)
        ));
    }

    Ok(LabelArgs {
        pr_number,
        minutes,
        repo,
    })
}

fn usage() -> String {
    "usage: modelstat label <pr-number> <minutes> [--repo <path>]\n  \
     e.g. modelstat label 412 90   — PR #412 took about an hour and a half"
        .to_string()
}

/// ISO-8601 now. The labels crate reads no clock of its own so it stays pure;
/// the caller supplies the instant.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn cmd_label(args: &[String]) -> ExitCode {
    let parsed = match parse_label_args(args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    let Some(repo) = resolve_repo_root(Some(&parsed.repo)) else {
        eprintln!(
            "modelstat label: `{}` is not inside a git repository",
            parsed.repo
        );
        return ExitCode::FAILURE;
    };

    // The slug is the store's key, so a repo with no `origin` has nowhere to
    // file the label — and would silently accumulate labels nothing can read.
    let Some(mined) = mine_repo_anchors(&repo, &AnchorConfig::default()) else {
        eprintln!(
            "modelstat label: could not read {repo} — no `origin` remote, or git could not be read"
        );
        return ExitCode::FAILURE;
    };

    // A label on a PR this history does not contain can never be turned into a
    // (units, minutes) pair, so it is not a label — it is a typo that would sit
    // in the file forever looking like progress toward the threshold.
    let all = merged_prs(&repo);
    let shas: BTreeMap<u64, String> = all
        .iter()
        .map(|p| (p.pr_number, p.merge_sha.clone()))
        .collect();
    if !shas.contains_key(&parsed.pr_number) {
        eprintln!(
            "modelstat label: #{} is not a merged PR in {}'s history \
             (searched {} merged PRs)",
            parsed.pr_number,
            mined.slug,
            shas.len()
        );
        return ExitCode::FAILURE;
    }

    let path = labels_path();
    let mut store = LabelStore::load(&path);
    let replacing = store
        .labels_for_repo(&mined.slug)
        .any(|(pr, _)| pr == parsed.pr_number);
    store.add_label(
        &mined.slug,
        parsed.pr_number,
        parsed.minutes,
        &now_iso(),
    );
    store.save(&path);

    let count = store.labels_for_repo(&mined.slug).count();
    println!(
        "{} #{} = {} min  ({} label{} for {})",
        if replacing { "updated" } else { "labelled" },
        parsed.pr_number,
        parsed.minutes,
        count,
        if count == 1 { "" } else { "s" },
        mined.slug
    );

    // Below the threshold this is a countdown; at or above it, the fit is real
    // and the honest thing to print is how badly it predicts. Refitting costs a
    // `git show` per label, so it only happens once it can succeed.
    let needed = MIN_LABELS.saturating_sub(count);
    if needed > 0 {
        let (plural, verb) = if needed == 1 { ("", "unlocks") } else { ("s", "unlock") };
        println!(
            "{needed} more label{plural} {verb} hours — \
             run `modelstat label <pr> <minutes>` again"
        );
        return ExitCode::SUCCESS;
    }
    match build_calibration(&repo, &mined.slug, &mined.anchors, &store, &shas) {
        Some(cal) => println!(
            "hours unlocked — ± {:.0}% (LOOCV, n={}), rank correlation {:.2}. \
             See them with `modelstat roi`",
            cal.median_abs_pct_error(),
            cal.n(),
            cal.spearman_rho()
        ),
        None => println!(
            "hours still locked — {count} labels are stored but fewer than {MIN_LABELS} \
             could be scored against this repo's history"
        ),
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_positionals_and_the_repo_flag() {
        let p = parse_label_args(&args(&["412", "90"])).unwrap();
        assert_eq!((p.pr_number, p.minutes, p.repo.as_str()), (412, 90, "."));

        let p = parse_label_args(&args(&["--repo", "/tmp/x", "#7", "45"])).unwrap();
        assert_eq!((p.pr_number, p.minutes, p.repo.as_str()), (7, 45, "/tmp/x"));

        let p = parse_label_args(&args(&["8", "30", "--repo=/tmp/y"])).unwrap();
        assert_eq!(p.repo, "/tmp/y");
    }

    #[test]
    fn rejects_zero_minutes_with_a_reason() {
        let e = parse_label_args(&args(&["412", "0"])).unwrap_err();
        assert!(e.contains("0 minutes"), "{e}");
    }

    #[test]
    fn rejects_absurd_minutes_and_says_how_absurd() {
        let e = parse_label_args(&args(&["412", "100000"])).unwrap_err();
        assert!(e.contains("69 days"), "{e}");
        // The ceiling itself is accepted; one past it is not.
        assert!(parse_label_args(&args(&["412", "10080"])).is_ok());
        assert!(parse_label_args(&args(&["412", "10081"])).is_err());
    }

    #[test]
    fn rejects_non_numbers_and_pr_zero() {
        assert!(parse_label_args(&args(&["four-twelve", "90"]))
            .unwrap_err()
            .contains("not a PR number"));
        assert!(parse_label_args(&args(&["412", "an hour"]))
            .unwrap_err()
            .contains("whole number of minutes"));
        assert!(parse_label_args(&args(&["0", "90"]))
            .unwrap_err()
            .contains("start at 1"));
        assert!(parse_label_args(&args(&["412", "-5"])).is_err());
    }

    #[test]
    fn missing_arguments_print_usage() {
        assert!(parse_label_args(&args(&[])).unwrap_err().contains("usage:"));
        assert!(parse_label_args(&args(&["412"]))
            .unwrap_err()
            .contains("usage:"));
        // `--repo`'s value is not mistaken for a positional.
        assert!(parse_label_args(&args(&["--repo", "/tmp/x", "412"]))
            .unwrap_err()
            .contains("usage:"));
    }

    #[test]
    fn stores_and_counts_labels_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("effort-labels.json");
        let mut store = LabelStore::load(&path);
        assert_eq!(store.labels_for_repo("org/repo").count(), 0);

        store.add_label("org/repo", 412, 90, "2026-08-09T10:00:00Z");
        store.add_label("org/repo", 413, 30, "2026-08-09T10:01:00Z");
        store.save(&path);

        let reloaded = LabelStore::load(&path);
        assert_eq!(reloaded.labels_for_repo("org/repo").count(), 2);
        assert_eq!(MIN_LABELS.saturating_sub(2), 6);
        // A relabel corrects rather than accumulates.
        let mut reloaded = reloaded;
        reloaded.add_label("org/repo", 412, 120, "2026-08-09T11:00:00Z");
        assert_eq!(reloaded.labels_for_repo("org/repo").count(), 2);
        assert_eq!(
            reloaded
                .labels_for_repo("org/repo")
                .find(|(pr, _)| *pr == 412)
                .unwrap()
                .1
                .minutes,
            120
        );
    }

    #[test]
    fn now_iso_is_a_parseable_instant() {
        assert!(chrono::DateTime::parse_from_rfc3339(&now_iso()).is_ok());
    }
}
