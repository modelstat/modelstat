//! The git-enrichment seam the M4 session-metadata pass drives — the injected
//! `resolveGit` / `checkPrOutcome` / `collectFilesChanged` of the TS
//! `buildSessionMetadata` bundled behind one trait (feature §7.4).
//!
//! The pass ([`modelstat-pipeline`]) is generic over this trait so it stays pure
//! + unit-testable (fakes for the git I/O) and never shells out itself. The real
//! collector wires [`RealGitEnrichment`], which fronts the (process-lifetime
//! cached) [`GitResolver`] plus the two stateless git-history reads. All three
//! calls are best-effort: `None` means "no signal", never an error the pass must
//! handle — a git miss just leaves that channel unenriched.

use modelstat_wire::GitContext;

use crate::git::GitResolver;
use crate::git_files::{collect_files_changed, FileChange};
use crate::git_outcome::{check_pull_request_outcome, PrOutcome};

/// The three best-effort git reads the session-metadata pass needs. Bundled
/// (rather than three separate injected closures like the TS `opts`) because they
/// share one repo-on-disk context and, in the real impl, one resolver cache —
/// and "absent" vs "present-but-returns-None" are observationally identical to
/// the pass, so a single trait covers the whole TS optionality space.
pub trait GitEnrichment {
    /// Authoritative git context for a cwd (remote slug/host/branch), or None.
    fn resolve_git(&mut self, cwd: Option<&str>) -> Option<GitContext>;
    /// The local verified-outcome of a PR whose repo is at `cwd`, or None.
    fn check_pr_outcome(&mut self, cwd: &str, pr_number: u64) -> Option<PrOutcome>;
    /// The files changed in `cwd` across [`since`, `until`] (ISO-8601), or None.
    fn collect_files_changed(
        &mut self,
        cwd: &str,
        since: &str,
        until: &str,
    ) -> Option<Vec<FileChange>>;
}

/// The production [`GitEnrichment`] — the real `git` subprocess reads. Fronts a
/// [`GitResolver`] so repeated cwds in a batch resolve once (matching the TS
/// module-level cache).
#[derive(Default)]
pub struct RealGitEnrichment {
    pub resolver: GitResolver,
}

impl RealGitEnrichment {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GitEnrichment for RealGitEnrichment {
    fn resolve_git(&mut self, cwd: Option<&str>) -> Option<GitContext> {
        self.resolver.resolve(cwd)
    }
    fn check_pr_outcome(&mut self, cwd: &str, pr_number: u64) -> Option<PrOutcome> {
        check_pull_request_outcome(cwd, pr_number)
    }
    fn collect_files_changed(
        &mut self,
        cwd: &str,
        since: &str,
        until: &str,
    ) -> Option<Vec<FileChange>> {
        collect_files_changed(cwd, since, until)
    }
}
