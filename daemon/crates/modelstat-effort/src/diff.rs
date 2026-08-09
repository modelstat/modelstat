//! What one merged PR actually changed, read from the LOCAL repo.
//!
//! Everything here is on-device. Paths are read — they are the only reliable
//! way to tell a lockfile from a hand-written parser — and then dropped: a
//! [`DiffFeatures`] holds counts, file extensions and a structure-only excerpt,
//! and deliberately does NOT implement `Serialize`, so no code path can put one
//! on a wire by accident. The serializable types this crate exposes are the
//! numeric report shapes in [`crate::units`] and [`crate::calibrate`].
//!
//! Best-effort like every other git read in this workspace: bounded
//! (`--format=` so git never even prints the message, a byte ceiling on stdout,
//! a file-count ceiling) and a 4s timeout, `None` on any failure.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Per-git-call ceiling — the same 4s class as
/// `modelstat_parsers::git_outcome::check_pull_request_outcome`.
const GIT_TIMEOUT: Duration = Duration::from_millis(4_000);

/// Stdout ceiling for the unified diff. A monorepo merge can be hundreds of
/// megabytes; past this we stop reading and let git die on the closed pipe.
const DIFF_MAX_BYTES: usize = 200 * 1024;

/// Stdout ceiling for the numstat. One row per file, so this is ~4k files.
const NUMSTAT_MAX_BYTES: usize = 256 * 1024;

/// Numstat rows aggregated. A PR that touched more files than this is already
/// off the scale the anchors calibrate, so the tail buys nothing.
const MAX_FILES: usize = 2_000;

/// Extensions kept in [`DiffFeatures::languages`], most files first.
const MAX_LANGS: usize = 12;

/// Byte ceiling on the structure-only excerpt handed to the judge.
pub const EXCERPT_MAX_BYTES: usize = 8 * 1024;

/// What a path is, by convention. Effort does not scale with churn alone: 600
/// lines of regenerated lockfile and 600 lines of new consensus code are the
/// same `lines_added` and nowhere near the same work.
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
/// Intentionally NOT `Serialize`. See the module docs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiffFeatures {
    pub files_changed: u32,
    pub lines_added: u64,
    pub lines_deleted: u64,
    /// `@@` hunks across the (bounded) diff — a proxy for how scattered the
    /// change is, which two PRs of identical churn can differ wildly on.
    pub hunks: u32,
    /// `(extension, file count)`, most files first. Extension only — never a
    /// path, never a filename.
    pub languages: Vec<(String, u32)>,
    /// Churn (`added + deleted`) attributed to each [`PathClass`].
    pub test_lines: u64,
    pub config_lines: u64,
    pub doc_lines: u64,
    pub generated_lines: u64,
    /// Structure-only rendering of the diff for the judge: hunk headers with
    /// their line counts and per-line SHAPES (sign, indent, length, kind).
    /// Contains no identifiers, no paths and no source text — see
    /// [`structure_excerpt`]. Local-only regardless.
    pub excerpt: String,
}

impl DiffFeatures {
    /// Total churn — the unit the anchor population speaks.
    ///
    /// Raw, undiscounted. The per-class weighting that turns this into an
    /// effort signal lives in [`crate::units`], because it needs weights an
    /// `AnchorPr` cannot carry.
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

/// The post-image path of a `diff --git a/X b/Y` header, or `None`. Pure.
fn diff_header_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("diff --git ")?;
    // Quoted paths (git's C-style quoting for non-ASCII) are left alone: we
    // only need the extension and the class, and a mis-parse is a `Source`
    // guess, not a leak.
    let (_, b) = rest.rsplit_once(" b/")?;
    Some(b)
}

/// One changed line, as a SHAPE: sign, indent width, visible length, kind.
///
/// This is the whole privacy argument for the excerpt. `+8/42x` says "an added
/// line, indented eight columns, forty-two characters wide, code" and cannot be
/// turned back into the line. Tabs count as four columns.
fn line_shape(sign: char, body: &str) -> String {
    let indent: usize = body
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum();
    let trimmed = body.trim();
    let len = trimmed.chars().count().min(999);
    let kind = if trimmed.is_empty() {
        'b'
    } else if trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("--")
        || trimmed.starts_with("<!--")
        || trimmed.starts_with(';')
    {
        'c'
    } else {
        'x'
    };
    format!("{sign}{indent}/{len}{kind}\n")
}

/// Render a unified diff as structure only, capped at `cap` bytes. Pure.
///
/// Keeps: a per-file line naming the file's [`PathClass`] and extension, hunk
/// headers truncated at the second `@@` (the tail git appends there is the
/// enclosing function's source text), and one shape per changed line. Drops
/// everything else — paths, identifiers, literals, context lines.
pub fn structure_excerpt(diff: &str, cap: usize) -> String {
    let mut out = String::with_capacity(cap.min(4096));
    let mut file_no = 0u32;
    for line in diff.lines() {
        let piece = if let Some(path) = diff_header_path(line) {
            file_no += 1;
            let file = path.rsplit('/').next().unwrap_or(path);
            let ext = extension(&file.to_ascii_lowercase()).unwrap_or_else(|| "none".into());
            let class = match classify_path(path) {
                PathClass::Test => "test",
                PathClass::Generated => "generated",
                PathClass::Doc => "doc",
                PathClass::Config => "config",
                PathClass::Source => "source",
            };
            format!("file {file_no} {class} .{ext}\n")
        } else if line.starts_with("@@ ") {
            // `@@ -a,b +c,d @@ fn whatever(` → `@@ -a,b +c,d @@`.
            let head = match line[3..].find("@@") {
                Some(i) => &line[..3 + i + 2],
                None => line,
            };
            format!("{head}\n")
        } else if let Some(body) = line.strip_prefix('+') {
            if line.starts_with("+++") {
                continue;
            }
            line_shape('+', body)
        } else if let Some(body) = line.strip_prefix('-') {
            if line.starts_with("---") {
                continue;
            }
            line_shape('-', body)
        } else {
            continue;
        };
        if out.len() + piece.len() > cap {
            out.push_str("…truncated\n");
            break;
        }
        out.push_str(&piece);
    }
    out
}

/// Fold a numstat and a unified diff into [`DiffFeatures`]. Pure — this is the
/// half that is unit-tested; [`diff_features`] is the git call around it.
pub fn features_from(numstat: &str, diff: &str) -> DiffFeatures {
    let rows = parse_numstat(numstat);
    let mut f = DiffFeatures {
        files_changed: rows.len() as u32,
        excerpt: structure_excerpt(diff, EXCERPT_MAX_BYTES),
        hunks: diff.lines().filter(|l| l.starts_with("@@ ")).count() as u32,
        ..Default::default()
    };
    let mut langs: Vec<(String, u32)> = Vec::new();
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
        let file = row.path.rsplit('/').next().unwrap_or(row.path);
        if let Some(ext) = extension(&file.to_ascii_lowercase()) {
            match langs.iter_mut().find(|(e, _)| *e == ext) {
                Some((_, n)) => *n += 1,
                None => langs.push((ext, 1)),
            }
        }
    }
    // Most files first, extension name as the tiebreak so the prompt built from
    // this is byte-identical for the same diff on every machine.
    langs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    langs.truncate(MAX_LANGS);
    f.languages = langs;
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
    // The diff is a nice-to-have: features stand without it (only `hunks` and
    // the excerpt come from here), so a timeout on the big read degrades rather
    // than fails.
    let diff = run_git_bounded(
        &[
            "show",
            "-m",
            "--first-parent",
            "--no-renames",
            "--unified=3",
            "--no-color",
            "--format=",
            merge_sha,
        ],
        cwd,
        DIFF_MAX_BYTES,
    )
    .unwrap_or_default();
    Some(features_from(&numstat, &diff))
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
            "tests/effort.rs",
            "src/foo/__tests__/bar.ts",
            "cmd/erpc/main_test.go",
            "packages/core/src/roi.test.ts",
            "spec/models/user_spec.rb",
            "test_calibrate.py",
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
            "daemon/crates/modelstat-effort/src/calibrate.rs",
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
        let f = features_from(numstat, "");
        assert_eq!(f.files_changed, 6);
        assert_eq!(f.lines_added, 737);
        assert_eq!(f.lines_deleted, 426);
        assert_eq!(f.test_lines, 30);
        assert_eq!(f.config_lines, 10);
        assert_eq!(f.generated_lines, 1000);
        assert_eq!(f.doc_lines, 3);
        assert_eq!(f.churn(), 1163);
        assert_eq!(f.languages[0], ("rs".to_string(), 2));
    }

    const SAMPLE_DIFF: &str = "\
diff --git a/src/secret/token_store.rs b/src/secret/token_store.rs
index a2796573..a476f67e 100644
--- a/src/secret/token_store.rs
+++ b/src/secret/token_store.rs
@@ -9,6 +9,7 @@ impl TokenStore {
 	let existing = self.load();
+	let api_key = \"sk-live-DEADBEEF\";
-        drop(existing);
+
+// explain the swap
";

    #[test]
    fn excerpt_keeps_shape_and_drops_every_identifier() {
        let ex = structure_excerpt(SAMPLE_DIFF, EXCERPT_MAX_BYTES);
        for leak in [
            "sk-live-DEADBEEF",
            "token_store",
            "secret",
            "TokenStore",
            "api_key",
            "explain the swap",
            "existing",
        ] {
            assert!(!ex.contains(leak), "excerpt leaked {leak:?}:\n{ex}");
        }
        assert!(ex.contains("file 1 source .rs"));
        assert!(ex.contains("@@ -9,6 +9,7 @@"), "hunk tail must be cut:\n{ex}");
        assert!(!ex.contains("impl"), "hunk tail must be cut:\n{ex}");
        // tab indent = 4 columns, 33 visible chars, code.
        assert!(ex.contains("+4/33x"), "{ex}");
        assert!(ex.contains("-8/15x"), "{ex}");
        assert!(ex.contains("+0/0b"), "blank added line:\n{ex}");
        assert!(ex.contains("+0/19c"), "comment added line:\n{ex}");
    }

    #[test]
    fn excerpt_respects_its_cap() {
        let big = SAMPLE_DIFF.repeat(500);
        let ex = structure_excerpt(&big, 512);
        assert!(ex.len() <= 512 + "…truncated\n".len());
        assert!(ex.ends_with("…truncated\n"));
    }

    #[test]
    fn hunks_counted_from_diff() {
        let f = features_from("1\t1\tsrc/a.rs", SAMPLE_DIFF);
        assert_eq!(f.hunks, 1);
    }

    #[test]
    fn missing_sha_yields_none_not_panic() {
        // A directory that is a repo (this workspace) but a sha that is not.
        assert!(diff_features(".", "0000000000000000000000000000000000000000").is_none());
        assert!(diff_features("/nonexistent-path-modelstat-effort", "HEAD").is_none());
    }
}
