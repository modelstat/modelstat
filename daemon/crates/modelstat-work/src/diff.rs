//! What one merged PR actually changed, read from the LOCAL repo.
//!
//! Everything here is on-device. Paths are read — they are the only reliable
//! way to tell a lockfile from a hand-written parser — and then dropped: a
//! [`DiffFeatures`] holds counts and nothing else, and deliberately does NOT
//! implement `Serialize`, so no code path can put one on a wire by accident.
//! What leaves this crate serialized are the spend counts in
//! [`crate::attribution`].
//!
//! Best-effort like every other git read in this workspace: ONE bounded call
//! (`--numstat --format=`, so git neither prints the message nor generates a
//! patch), a byte ceiling on stdout, a file-count ceiling and a 4s timeout,
//! `None` on any failure.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Per-git-call ceiling — the same 4s class as
/// `modelstat_parsers::git_outcome::check_pull_request_outcome`.
const GIT_TIMEOUT: Duration = Duration::from_millis(4_000);

/// Stdout ceiling for the numstat. One row per file, so this is ~4k files.
const NUMSTAT_MAX_BYTES: usize = 256 * 1024;

/// Numstat rows aggregated. A PR that touched more files than this is churn on
/// a scale no per-file classification can say anything useful about.
const MAX_FILES: usize = 2_000;

/// What a path is, by convention. Churn alone does not say what changed: 600
/// lines of regenerated lockfile and 600 lines of new consensus code are the
/// same `lines_added` and plainly not the same event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    /// Tests, specs, fixtures, testdata.
    Test,
    /// Lockfiles, minified bundles, protobuf/codegen output, build directories.
    Generated,
    /// Markdown, rst, licences, changelogs.
    Doc,
    /// Manifests, CI, dotfiles, infra.
    Config,
    /// Everything else — the code someone actually wrote.
    Source,
}

/// One `git show --numstat` row. Borrowed from the stdout buffer: the path is
/// looked at, classified, and never copied into [`DiffFeatures`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatRow<'a> {
    /// `None` for a binary file (git writes `-`).
    pub added: Option<u64>,
    pub deleted: Option<u64>,
    pub path: &'a str,
}

/// The local, never-transmitted shape of one PR's diff.
///
/// Counts only, and every one of them is recountable by hand from
/// `git show --numstat`. Intentionally NOT `Serialize`: see the module docs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiffFeatures {
    pub files_changed: u32,
    pub lines_added: u64,
    pub lines_deleted: u64,
    /// Churn (`added + deleted`) attributed to each [`PathClass`]. Reported
    /// apart rather than discounted against each other: which classes a change
    /// landed in is measured here, what each one is worth is the reader's call.
    pub test_lines: u64,
    pub config_lines: u64,
    pub doc_lines: u64,
    pub generated_lines: u64,
}

impl DiffFeatures {
    /// Total churn — added plus deleted, raw and undiscounted.
    pub fn churn(&self) -> u64 {
        self.lines_added.saturating_add(self.lines_deleted)
    }
}

/// The file extension, lowercased, or `None`. A dotfile with no second dot
/// (`.gitignore`) has no extension — the leading dot is not one.
fn extension(file_name: &str) -> Option<String> {
    let stem = file_name.strip_prefix('.').unwrap_or(file_name);
    let (_, ext) = stem.rsplit_once('.')?;
    if ext.is_empty() || ext.len() > 12 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Classify a repo-relative path. Pure.
///
/// Precedence is Generated → Test → Doc → Config → Source, and it is load
/// bearing in that order: `tests/__snapshots__/x.snap` is generated output that
/// happens to live under `tests/`, and counting it as hand-written test work
/// would inflate every snapshot-heavy PR.
pub fn classify_path(path: &str) -> PathClass {
    let lower = path.to_ascii_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let segs: Vec<&str> = lower.split('/').filter(|s| !s.is_empty()).collect();
    let dirs = &segs[..segs.len().saturating_sub(1)];
    let ext = extension(file);
    let ext = ext.as_deref().unwrap_or("");

    const LOCKFILES: &[&str] = &[
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "cargo.lock",
        "poetry.lock",
        "uv.lock",
        "go.sum",
        "gemfile.lock",
        "composer.lock",
        "bun.lockb",
        "flake.lock",
        "podfile.lock",
    ];
    const GENERATED_DIRS: &[&str] = &[
        "generated",
        "__generated__",
        "node_modules",
        "vendor",
        "dist",
        "build",
        "target",
        "__pycache__",
        ".next",
        "__snapshots__",
    ];
    const GENERATED_SUFFIXES: &[&str] = &[
        ".min.js",
        ".min.css",
        ".map",
        ".snap",
        ".pb.go",
        ".pb.cc",
        ".pb.h",
        "_pb2.py",
        ".g.dart",
        ".generated.ts",
        "_generated.go",
    ];
    if LOCKFILES.contains(&file)
        || file.contains("generated")
        || GENERATED_SUFFIXES.iter().any(|s| file.ends_with(s))
        || dirs.iter().any(|d| GENERATED_DIRS.contains(d))
    {
        return PathClass::Generated;
    }

    const TEST_DIRS: &[&str] = &[
        "test", "tests", "spec", "specs", "__tests__", "testdata", "e2e", "fixtures", "__mocks__",
        "it",
    ];
    let test_file = file == "conftest.py"
        || file.starts_with("test_")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.contains("_test.")
        || file.contains("_spec.")
        || file.ends_with("test.java")
        || file.ends_with("tests.cs");
    if test_file || dirs.iter().any(|d| TEST_DIRS.contains(d)) {
        return PathClass::Test;
    }

    const DOC_EXTS: &[&str] = &["md", "mdx", "rst", "adoc", "txt"];
    const DOC_DIRS: &[&str] = &["doc", "docs"];
    const DOC_FILES: &[&str] = &["license", "notice", "authors", "changelog", "readme"];
    if DOC_EXTS.contains(&ext)
        || DOC_FILES.contains(&file)
        || dirs.iter().any(|d| DOC_DIRS.contains(d))
    {
        return PathClass::Doc;
    }

    const CONFIG_EXTS: &[&str] = &[
        "json",
        "yaml",
        "yml",
        "toml",
        "ini",
        "cfg",
        "conf",
        "properties",
        "env",
        "tf",
        "tfvars",
        "gradle",
        "xml",
        "plist",
        "lock",
    ];
    const CONFIG_FILES: &[&str] = &[
        "dockerfile",
        "makefile",
        "justfile",
        "procfile",
        "rakefile",
        "brewfile",
    ];
    const CONFIG_DIRS: &[&str] = &[".github", ".circleci", ".vscode", ".idea", ".husky"];
    if CONFIG_EXTS.contains(&ext)
        || CONFIG_FILES.contains(&file)
        || file.starts_with('.')
        || dirs.iter().any(|d| CONFIG_DIRS.contains(d))
    {
        return PathClass::Config;
    }

    PathClass::Source
}

/// Parse `git show --numstat` stdout. Pure.
///
/// A row is `<added>\t<deleted>\t<path>`; a binary file's `-\t-` row keeps its
/// place in the file count but carries no line signal. Anything that is not a
/// well-formed row (blank lines, the empty line `--format=` leaves behind) is
/// skipped rather than guessed at.
///
/// Renames are not handled because the caller passes `--no-renames`, which
/// makes git emit a plain delete row and a plain add row instead of the
/// `old => new` path form this would otherwise have to unpick.
pub fn parse_numstat(stdout: &str) -> Vec<NumstatRow<'_>> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Some((added, rest)) = line.split_once('\t') else {
            continue;
        };
        let Some((deleted, path)) = rest.split_once('\t') else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        let num = |s: &str| -> Option<Option<u64>> {
            if s == "-" {
                Some(None)
            } else {
                s.parse::<u64>().ok().map(Some)
            }
        };
        let (Some(added), Some(deleted)) = (num(added), num(deleted)) else {
            continue;
        };
        out.push(NumstatRow {
            added,
            deleted,
            path,
        });
        if out.len() >= MAX_FILES {
            break;
        }
    }
    out
}

/// Fold a numstat into [`DiffFeatures`]. Pure — this is the half that is
/// unit-tested; [`diff_features`] is the git call around it.
pub fn features_from(numstat: &str) -> DiffFeatures {
    let rows = parse_numstat(numstat);
    let mut f = DiffFeatures {
        files_changed: rows.len() as u32,
        ..Default::default()
    };
    for row in &rows {
        // Saturating, like the totals below: a numstat is untrusted input, and
        // this crate's contract is "degrade, never panic".
        let churn = row.added.unwrap_or(0).saturating_add(row.deleted.unwrap_or(0));
        f.lines_added = f.lines_added.saturating_add(row.added.unwrap_or(0));
        f.lines_deleted = f.lines_deleted.saturating_add(row.deleted.unwrap_or(0));
        match classify_path(row.path) {
            PathClass::Test => f.test_lines = f.test_lines.saturating_add(churn),
            PathClass::Config => f.config_lines = f.config_lines.saturating_add(churn),
            PathClass::Doc => f.doc_lines = f.doc_lines.saturating_add(churn),
            PathClass::Generated => f.generated_lines = f.generated_lines.saturating_add(churn),
            PathClass::Source => {}
        }
    }
    f
}

/// Read one merged commit's diff features from the local repo. `None` when
/// `cwd` is not a readable repo, the sha is unknown, or git times out.
///
/// `--format=` is not cosmetic: it is why no commit message and no author name
/// is ever read into this process. `-m --first-parent` gives a merge commit's
/// diff against mainline (i.e. what the PR landed) and is a no-op on a squash
/// merge's single parent.
pub fn diff_features(cwd: &str, merge_sha: &str) -> Option<DiffFeatures> {
    let numstat = run_git_bounded(
        &[
            "show",
            "-m",
            "--first-parent",
            "--no-renames",
            "--numstat",
            "--format=",
            merge_sha,
        ],
        cwd,
        NUMSTAT_MAX_BYTES,
    )?;
    Some(features_from(&numstat))
}

/// Run `git` in `cwd`, reading at most `max_bytes` of stdout, killing the child
/// past [`GIT_TIMEOUT`]. `None` on spawn failure, timeout, or empty output.
///
/// The exit status is deliberately not checked: once the reader has taken
/// `max_bytes` it drops the pipe, git dies on EPIPE, and a non-zero status
/// there means "we stopped listening", not "the read failed". Empty stdout is
/// the real failure signal — a bad sha writes to stderr, which is `/dev/null`.
fn run_git_bounded(args: &[&str], cwd: &str, max_bytes: usize) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(8 * 1024);
        let _ = (&mut stdout).take(max_bytes as u64).read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) if start.elapsed() >= GIT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                break true;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break true;
            }
        }
    };
    let buf = reader.join().ok()?;
    if timed_out || buf.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_test_paths() {
        for p in [
            "tests/work.rs",
            "src/foo/__tests__/bar.ts",
            "cmd/erpc/main_test.go",
            "packages/core/src/roi.test.ts",
            "spec/models/user_spec.rb",
            "test_attribution.py",
            "internal/testdata/big.json",
            "e2e/checkout.ts",
        ] {
            assert_eq!(classify_path(p), PathClass::Test, "{p}");
        }
    }

    #[test]
    fn classifies_generated_paths() {
        for p in [
            "pnpm-lock.yaml",
            "Cargo.lock",
            "go.sum",
            "web/static/app.min.js",
            "src/api/schema.generated.ts",
            "proto/service.pb.go",
            "dist/bundle.js",
            "tests/__snapshots__/view.snap",
        ] {
            assert_eq!(classify_path(p), PathClass::Generated, "{p}");
        }
    }

    #[test]
    fn classifies_doc_and_config_paths() {
        for p in ["README.md", "docs/adr/0001-anchors.rst", "LICENSE"] {
            assert_eq!(classify_path(p), PathClass::Doc, "{p}");
        }
        for p in [
            "Cargo.toml",
            ".gitignore",
            ".github/workflows/ci.yml",
            "biome.json",
            "Makefile",
            "infra/main.tf",
            ".vscode/launch.json",
        ] {
            assert_eq!(classify_path(p), PathClass::Config, "{p}");
        }
    }

    #[test]
    fn classifies_everything_else_as_source() {
        for p in [
            "common/config.go",
            "daemon/crates/modelstat-work/src/attribution.rs",
            "app/models/user.rb",
        ] {
            assert_eq!(classify_path(p), PathClass::Source, "{p}");
        }
    }

    #[test]
    fn parses_numstat_rows_and_skips_junk() {
        let out = "\n12\t3\tsrc/lib.rs\n-\t-\tassets/logo.png\nnot a row\n0\t9\tREADME.md\n";
        let rows = parse_numstat(out);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].added, Some(12));
        assert_eq!(rows[0].deleted, Some(3));
        assert_eq!(rows[0].path, "src/lib.rs");
        assert_eq!(rows[1].added, None, "binary row keeps its place, not its lines");
        assert_eq!(rows[2].path, "README.md");
    }

    #[test]
    fn features_split_churn_by_path_class() {
        let numstat = "\
100\t20\tsrc/engine.rs
30\t0\ttests/engine.rs
5\t5\tCargo.toml
600\t400\tCargo.lock
2\t1\tdocs/design.md
-\t-\tassets/icon.png
";
        let f = features_from(numstat);
        assert_eq!(f.files_changed, 6);
        assert_eq!(f.lines_added, 737);
        assert_eq!(f.lines_deleted, 426);
        assert_eq!(f.test_lines, 30);
        assert_eq!(f.config_lines, 10);
        assert_eq!(f.generated_lines, 1000);
        assert_eq!(f.doc_lines, 3);
        assert_eq!(f.churn(), 1163);
    }

    #[test]
    fn a_single_row_without_a_trailing_newline_still_parses() {
        // What git emits for a one-file merge, and the only case where the last
        // row is not newline-terminated.
        let f = features_from("1\t1\tsrc/a.rs");
        assert_eq!((f.files_changed, f.lines_added, f.lines_deleted), (1, 1, 1));
        assert_eq!(f.churn(), 2);
        assert_eq!(f.test_lines, 0, "src/ is source, not test");
    }

    #[test]
    fn missing_sha_yields_none_not_panic() {
        // A directory that is a repo (this workspace) but a sha that is not.
        assert!(diff_features(".", "0000000000000000000000000000000000000000").is_none());
        assert!(diff_features("/nonexistent-path-modelstat-work", "HEAD").is_none());
    }
}
