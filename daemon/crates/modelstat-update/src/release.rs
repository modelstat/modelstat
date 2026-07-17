//! The release verdict (from the heartbeat's `daemon_release`, §13/§17.1) + the
//! GitHub-Releases archive URL for this target triple (plan D9).

use std::time::Duration;

use serde::Deserialize;

/// GitHub repo the releases live under (plan D9).
const RELEASES_BASE: &str = "https://github.com/modelstat/modelstat/releases/download";
/// The GitHub API `releases/latest` endpoint (manual-upgrade + self-hosted-box
/// path — they have no heartbeat verdict to key off).
const LATEST_API: &str = "https://api.github.com/repos/modelstat/modelstat/releases/latest";

/// The server's release verdict for this daemon. Three values (TS parity):
/// `ok` (up to date), `update_available` (newer exists), `upgrade_required`
/// (server forcing the floor). Unknown strings are treated as `ok` — the server
/// owns this decision and a value we don't recognize must not trigger a swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseVerdict {
    Ok,
    UpdateAvailable,
    UpgradeRequired,
}

impl ReleaseVerdict {
    pub fn parse(s: &str) -> Self {
        match s {
            "update_available" => ReleaseVerdict::UpdateAvailable,
            "upgrade_required" => ReleaseVerdict::UpgradeRequired,
            _ => ReleaseVerdict::Ok,
        }
    }
    /// Whether this verdict should drive an update attempt.
    pub fn wants_update(self) -> bool {
        matches!(
            self,
            ReleaseVerdict::UpdateAvailable | ReleaseVerdict::UpgradeRequired
        )
    }
    pub fn is_required(self) -> bool {
        self == ReleaseVerdict::UpgradeRequired
    }
}

/// The `daemon_release` object carried by every heartbeat response (§17.1).
/// `min` is received for completeness but the server precomputes `verdict`, so
/// the client keys off `verdict` + `latest` (TS parity — `min` is ignored).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DaemonRelease {
    #[serde(default)]
    pub verdict: Option<String>,
    #[serde(default)]
    pub min: Option<String>,
    #[serde(default)]
    pub latest: Option<String>,
}

impl DaemonRelease {
    pub fn verdict(&self) -> ReleaseVerdict {
        ReleaseVerdict::parse(self.verdict.as_deref().unwrap_or("ok"))
    }
}

/// This build's target triple (e.g. `x86_64-apple-darwin`), captured by
/// `build.rs`. Names the per-platform release archive.
pub fn target_triple() -> &'static str {
    env!("MODELSTAT_TARGET_TRIPLE")
}

/// Archive extension per platform (plan D9): `.zip` on Windows, `.tar.gz` else.
fn archive_ext() -> &'static str {
    if cfg!(windows) {
        "zip"
    } else {
        "tar.gz"
    }
}

/// Strip a `daemon-` tag prefix (and a stray leading `v`) to the bare semver —
/// the heartbeat may report either `daemon-1.2.3` or `1.2.3`.
pub fn bare_version(v: &str) -> &str {
    v.strip_prefix("daemon-").unwrap_or(v).trim_start_matches('v')
}

/// The download URL for a version's archive on THIS platform (plan D9):
/// `…/releases/download/daemon-<v>/modelstat-<v>-<triple>.<ext>`, each archive
/// carrying both binaries (+ tray on macOS).
pub fn archive_url(version: &str, triple: &str) -> String {
    let v = bare_version(version);
    format!(
        "{RELEASES_BASE}/daemon-{v}/modelstat-{v}-{triple}.{ext}",
        ext = archive_ext()
    )
}

/// The daemon-release tag names are `daemon-<semver>`, so a `latest` release
/// whose tag is `daemon-1.4.2` yields `1.4.2`.
#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// Resolve the latest published version from the GitHub API (bare semver), or
/// `None` on any failure — the caller reports "couldn't resolve latest".
pub async fn resolve_latest_version() -> Option<String> {
    let resp = reqwest::Client::new()
        .get(LATEST_API)
        .header("User-Agent", "modelstat-updater")
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let latest: LatestRelease = resp.json().await.ok()?;
    Some(bare_version(&latest.tag_name).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_parsing_and_flags() {
        assert_eq!(ReleaseVerdict::parse("ok"), ReleaseVerdict::Ok);
        assert_eq!(
            ReleaseVerdict::parse("update_available"),
            ReleaseVerdict::UpdateAvailable
        );
        assert_eq!(
            ReleaseVerdict::parse("upgrade_required"),
            ReleaseVerdict::UpgradeRequired
        );
        // Unknown ⇒ Ok (never a swap on a value we don't recognize).
        assert_eq!(ReleaseVerdict::parse("weird"), ReleaseVerdict::Ok);
        assert!(!ReleaseVerdict::Ok.wants_update());
        assert!(ReleaseVerdict::UpdateAvailable.wants_update());
        assert!(ReleaseVerdict::UpgradeRequired.is_required());
    }

    #[test]
    fn release_deserializes_and_ignores_min() {
        let r: DaemonRelease =
            serde_json::from_str(r#"{"verdict":"update_available","min":"1.0.0","latest":"1.4.2"}"#)
                .unwrap();
        assert_eq!(r.verdict(), ReleaseVerdict::UpdateAvailable);
        assert_eq!(r.latest.as_deref(), Some("1.4.2"));
        // Absent fields default cleanly.
        let empty: DaemonRelease = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.verdict(), ReleaseVerdict::Ok);
    }

    #[test]
    fn archive_url_normalizes_version_and_names_the_triple() {
        let u = archive_url("daemon-1.4.2", "x86_64-apple-darwin");
        assert!(
            u.contains("/download/daemon-1.4.2/modelstat-1.4.2-x86_64-apple-darwin."),
            "{u}"
        );
        // Bare + v-prefixed inputs normalize the same way.
        assert_eq!(bare_version("daemon-1.4.2"), "1.4.2");
        assert_eq!(bare_version("v1.4.2"), "1.4.2");
        assert_eq!(bare_version("1.4.2"), "1.4.2");
        // Extension is platform-correct.
        let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
        assert!(u.ends_with(ext), "{u}");
    }
}
