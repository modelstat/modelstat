//! `modelstat redactor [local|cloud|self-hosted]` — where turns get SCRUBBED.
//!
//! Its own setting, separate from `modelstat mode` (where turns get SUMMARISED),
//! because they are separate questions with different answers for most people:
//! scrub on my machine, summarise on yours. One setting could not say that.
//!
//! Only `local` is implemented, and the other two are refused rather than quietly
//! accepted. Both of them mean raw, unscrubbed text leaves the machine so that
//! something else can scrub it — the exact egress the local redactor exists to
//! prevent — so a setting that appeared to work while doing that would be the worst
//! kind of footgun. They stay in the vocabulary because the wire and the config
//! already carry them and the server side is being built; the CLI says so plainly.

use std::process::ExitCode;

use modelstat_ingest::{state, Config};

/// Plain-language copy per redactor mode.
fn redactor_info(mode: &str) -> (&'static str, &'static str) {
    match mode {
        "local" => (
            "Local (default) — this machine scrubs, before anything is sent",
            "names, emails, phone numbers, addresses, account numbers and secrets are \
             removed on-device; nothing unscrubbed leaves, and a turn the redactor \
             cannot read is held rather than sent",
        ),
        "cloud" => (
            "Cloud — modelstat's servers would scrub",
            "NOT AVAILABLE: this requires sending your raw, unscrubbed turns to us so \
             we can scrub them, which is what the local redactor exists to avoid",
        ),
        _ => (
            "Self-hosted — your org's own redaction engine would scrub",
            "NOT AVAILABLE: this requires sending raw, unscrubbed turns to that engine",
        ),
    }
}

pub fn cmd_redactor(config: &Config, args: &[String]) -> ExitCode {
    let requested = args.first().map(|s| s.trim().to_lowercase());
    let current = config.redactor_mode();

    // No argument: report, and say how the two settings relate.
    let Some(requested) = requested else {
        let (title, detail) = redactor_info(&current);
        println!("redactor: {current}");
        println!("  {title}");
        println!("  {detail}");
        println!("summariser: {}", config.summarizer_mode());
        println!("  change the summariser with `modelstat mode`");
        return ExitCode::SUCCESS;
    };

    let Some(mode) = state::parse_redactor_mode(Some(&requested)) else {
        eprintln!(
            "modelstat: unknown redactor `{requested}` — expected one of {}",
            state::REDACTOR_MODES.join(", ")
        );
        return ExitCode::from(2);
    };

    if mode != "local" {
        let (title, detail) = redactor_info(mode);
        eprintln!("modelstat: {title}");
        eprintln!("  {detail}");
        eprintln!("  the redactor stays `{current}`");
        return ExitCode::from(2);
    }

    if let Err(e) = state::set_redactor_mode(mode) {
        eprintln!("modelstat: could not save the redactor setting: {e}");
        return ExitCode::FAILURE;
    }
    println!("redactor: {mode}");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_has_copy_and_the_unavailable_ones_say_so() {
        for m in state::REDACTOR_MODES {
            let (title, detail) = redactor_info(m);
            assert!(!title.is_empty() && !detail.is_empty(), "{m}");
            if m != "local" {
                assert!(
                    detail.contains("NOT AVAILABLE"),
                    "a mode that would ship raw text must say it is unavailable: {m}"
                );
                assert!(
                    detail.contains("raw"),
                    "say what the cost actually is, not just that it is off: {m}"
                );
            }
        }
    }

    #[test]
    fn local_is_the_default_and_cloud_is_the_summariser_default() {
        // The pairing the product wants: scrub here, summarise there.
        assert_eq!(state::DEFAULT_REDACTOR_MODE, "local");
        assert_eq!(state::DEFAULT_SUMMARIZER_MODE, "cloud");
    }
}
