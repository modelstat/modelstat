//! Ground-truth probe: what does the local PR-outcome check say about a PR?
//!
//! Prints one JSON line per `<repo-path> <pr-number>` pair given on argv, so a
//! harness can diff the daemon's verdict against the forge's. Exists because a
//! merged PR was being reported as `merged: false` — a confident wrong answer,
//! which is worse than no answer.
//!
//!   cargo run -q -p modelstat-parsers --example pr_outcome_probe -- <path> <n> [<path> <n> …]

use modelstat_parsers::git_outcome::check_pull_request_outcome;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 || args.len() % 2 != 0 {
        eprintln!("usage: pr_outcome_probe <repo-path> <pr-number> [<repo-path> <pr-number> …]");
        std::process::exit(2);
    }
    for pair in args.chunks(2) {
        let (cwd, num) = (&pair[0], pair[1].parse::<u64>().unwrap_or(0));
        // `None` from the check = the read itself could not run (not a repo, git
        // failed). That is distinct from "ran and found nothing", which is what
        // the verdict inside `PrOutcome` reports.
        let out = check_pull_request_outcome(cwd, num);
        let verdict = match &out {
            None => "unreadable".to_string(),
            Some(o) => format!("{:?}", o.merged),
        };
        let merge_sha = out
            .as_ref()
            .and_then(|o| o.merge_sha.clone())
            .unwrap_or_default();
        println!(
            r#"{{"repo":"{cwd}","pr":{num},"verdict":"{verdict}","merge_sha":"{merge_sha}"}}"#
        );
    }
}
