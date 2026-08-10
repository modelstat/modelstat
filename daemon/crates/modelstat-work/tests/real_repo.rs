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

use modelstat_work::diff_features;

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
    let dir = std::env::temp_dir().join(format!("modelstat-work-{}", std::process::id()));
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
fn reads_a_real_commit_into_change_primitives() {
    let Some((dir, sha)) = scratch_repo() else {
        eprintln!("git unavailable — skipping real-repo test");
        return;
    };
    let cwd = dir.to_string_lossy().to_string();

    let f = diff_features(&cwd, &sha).expect("features for a commit that exists");
    assert_eq!(f.files_changed, 3, "{f:?}");
    assert_eq!(f.lines_added, 6, "{f:?}");
    assert_eq!(f.lines_deleted, 0, "{f:?}");
    assert_eq!(f.churn(), 6, "{f:?}");
    assert_eq!(f.test_lines, 1, "{f:?}");
    assert_eq!(f.generated_lines, 3, "Cargo.lock is generated: {f:?}");
    assert_eq!(f.doc_lines, 0, "README untouched in commit two: {f:?}");
    assert_eq!(f.config_lines, 0, "{f:?}");

    // Every count is recountable by hand off git's own numstat. That is the
    // whole claim these primitives make, so the test makes it against git
    // rather than against a remembered number.
    let numstat = git(
        &dir,
        &[
            "show",
            "-m",
            "--first-parent",
            "--no-renames",
            "--numstat",
            "--format=",
            &sha,
        ],
    )
    .expect("numstat");
    let (mut rows, mut added, mut deleted) = (0u32, 0u64, 0u64);
    for line in numstat.lines().filter(|l| !l.trim().is_empty()) {
        let mut cols = line.split('\t');
        added += cols.next().and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
        deleted += cols.next().and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
        rows += 1;
    }
    assert_eq!(
        (f.files_changed, f.lines_added, f.lines_deleted),
        (rows, added, deleted)
    );

    let _ = fs::remove_dir_all(&dir);
}
