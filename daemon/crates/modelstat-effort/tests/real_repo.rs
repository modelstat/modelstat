//! The one test that actually shells out to git: everything else in this crate
//! is pure and tested against fixed strings. This proves the `git show` flags
//! (`-m --first-parent --no-renames --numstat --format=`) really produce what
//! `diff::features_from` expects, on a repo built here, offline.
//!
//! Skipped rather than failed when git is unavailable — a missing git is an
//! environment fact, not a regression in this crate.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use modelstat_effort::{diff_features, estimate_pr_effort, Confidence, Scorer};
use modelstat_wire::AnchorPr;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn commit(dir: &Path, message: &str) -> Option<String> {
    git(dir, &["add", "-A"])?;
    git(
        dir,
        &[
            "-c",
            "user.name=modelstat-test",
            "-c",
            "user.email=test@modelstat.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            message,
        ],
    )?;
    git(dir, &["rev-parse", "HEAD"])
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, body).expect("write");
}

/// A throwaway repo with two commits; the second is the one under test.
/// `None` when git could not be driven at all.
fn scratch_repo() -> Option<(PathBuf, String)> {
    let dir = std::env::temp_dir().join(format!("modelstat-effort-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).ok()?;
    git(&dir, &["init", "-q"])?;

    write(&dir, "src/engine.rs", "fn a() {}\nfn b() {}\n");
    write(&dir, "tests/engine.rs", "fn t() {}\n");
    write(&dir, "Cargo.lock", "[[package]]\nname = \"x\"\n");
    write(&dir, "README.md", "# x\n");
    commit(&dir, "one")?;

    write(
        &dir,
        "src/engine.rs",
        "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n",
    );
    write(&dir, "tests/engine.rs", "fn t() {}\nfn u() {}\n");
    write(
        &dir,
        "Cargo.lock",
        "[[package]]\nname = \"x\"\nversion = \"1\"\nsource = \"registry\"\nchecksum = \"ab\"\n",
    );
    let sha = commit(&dir, "two")?;
    Some((dir, sha))
}

#[test]
fn reads_a_real_commit_and_estimates_against_real_anchors() {
    let Some((dir, sha)) = scratch_repo() else {
        eprintln!("git unavailable — skipping real-repo test");
        return;
    };
    let cwd = dir.to_string_lossy().to_string();

    let f = diff_features(&cwd, &sha).expect("features for a commit that exists");
    assert_eq!(f.files_changed, 3, "{f:?}");
    assert_eq!(f.lines_added, 6, "{f:?}");
    assert_eq!(f.lines_deleted, 0, "{f:?}");
    assert_eq!(f.test_lines, 1, "{f:?}");
    assert_eq!(f.generated_lines, 3, "Cargo.lock is generated: {f:?}");
    assert_eq!(f.doc_lines, 0, "README untouched in commit two: {f:?}");
    assert_eq!(f.hunks, 3, "{f:?}");
    assert_eq!(f.languages, vec![("rs".to_string(), 2), ("lock".to_string(), 1)]);
    for leak in ["engine.rs", "Cargo.lock", "fn c()", "registry", "checksum"] {
        assert!(!f.excerpt.contains(leak), "excerpt leaked {leak:?}:\n{}", f.excerpt);
    }
    // git orders the diff alphabetically, so assert on the class, not the index.
    assert!(f.excerpt.contains("source .rs"), "{}", f.excerpt);
    assert!(f.excerpt.contains("test .rs"), "{}", f.excerpt);
    assert!(f.excerpt.contains("generated .lock"), "{}", f.excerpt);

    // Twelve human anchors, 20..240 measured minutes.
    let anchors: Vec<AnchorPr> = (1..=12)
        .map(|i| AnchorPr {
            pr_number: i,
            merge_sha: format!("{i:040x}"),
            merged_at: "2026-01-01T00:00:00.000Z".into(),
            files_changed: 4,
            lines_added: i * 20,
            lines_deleted: i * 5,
            span_ms: Some(7_200_000),
            commit_count: Some(3),
            ai_assisted: false,
            active_minutes: Some(i as u32 * 20),
        })
        .collect();

    let scorer = |_: &str| -> Option<String> {
        Some(r#"{"category":"feature","novelty_0_1":0.4,"boilerplate_fraction_0_1":0.5,
                 "risk_domains":[],"relative_position_0_1":0.5}"#
            .to_string())
    };
    let e = estimate_pr_effort(&cwd, &sha, &anchors, Some(&scorer as &dyn Scorer))
        .expect("estimate for a readable commit");
    assert_eq!(e.anchor_n, 12);
    assert_eq!(e.confidence, Confidence::Moderate);
    // Median of 20..240 by tens: pos = 0.5 * 11 = 5.5 ⇒ (120 + 140) / 2.
    assert_eq!(e.p50_minutes, 130);
    assert!(e.p10_minutes < e.p50_minutes && e.p50_minutes < e.p90_minutes, "{e:?}");

    // No scorer at all still yields a real, ordered, low-confidence estimate.
    let fallback = estimate_pr_effort(&cwd, &sha, &anchors, None).expect("fallback estimate");
    assert_eq!(fallback.confidence, Confidence::Low);
    assert!(fallback.p10_minutes <= fallback.p50_minutes);
    assert!(fallback.p50_minutes <= fallback.p90_minutes);

    let _ = fs::remove_dir_all(&dir);
}
