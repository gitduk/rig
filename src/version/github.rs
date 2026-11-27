use std::process::{Command, Stdio};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::config::Host;

use super::fetch_json;

#[derive(Debug)]
pub struct Release {
    pub tag: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// `sha256:<hex>` when the host publishes it. Absent on old releases
    /// and always on Codeberg (Gitea) — callers must degrade to size-only.
    pub digest: Option<String>,
}

#[derive(Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    assets: Vec<AssetResponse>,
}

impl ReleaseResponse {
    fn release(self) -> Release {
        Release {
            tag: self.tag_name,
            assets: self
                .assets
                .into_iter()
                .map(|a| Asset {
                    name: a.name,
                    url: a.browser_download_url,
                    size: a.size,
                    digest: a.digest,
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct AssetResponse {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

/// `GITHUB_TOKEN`, then `GH_TOKEN`, then `gh auth token`; resolved once per
/// process. Unauthenticated, the API allows only 60 requests/hour per IP.
fn github_token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            ["GITHUB_TOKEN", "GH_TOKEN"]
                .into_iter()
                .filter_map(|var| std::env::var(var).ok())
                .chain(std::iter::once_with(gh_cli_token).flatten())
                .map(|t| t.trim().to_string())
                .find(|t| !t.is_empty())
        })
        .as_deref()
}

fn gh_cli_token() -> Option<String> {
    let output = Command::new("gh")
        .args(["auth", "token"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// The HTTP status a `fetch_json` error chain carries, if any.
fn status(e: &anyhow::Error) -> Option<u16> {
    e.chain()
        .find_map(|c| match c.downcast_ref::<ureq::Error>() {
            Some(ureq::Error::StatusCode(code)) => Some(*code),
            _ => None,
        })
}

/// GitHub answers an exhausted quota with 403 (primary) or 429 (secondary).
fn with_quota_hint(host: Host, token: Option<&str>, e: anyhow::Error) -> anyhow::Error {
    if host == Host::Github && token.is_none() && matches!(status(&e), Some(403 | 429)) {
        e.context(
            "GitHub API rate limit likely exhausted (60 requests/hour unauthenticated) \
             — set GITHUB_TOKEN or run `gh auth login`",
        )
    } else {
        e
    }
}

// Drafts need a push-access token to show up; the newest few suffice.
const FALLBACK_PAGE: usize = 5;

/// `/releases/latest` answers 404 once every release is marked prerelease, so
/// a 404 falls back to the newest non-draft entry of the release list.
pub fn latest_release(name: &str, host: Host) -> anyhow::Result<Release> {
    // (API base, the name of the list endpoint's page-size parameter)
    let (api, page_param, token) = match host {
        Host::Github => ("https://api.github.com/repos", "per_page", github_token()),
        Host::Codeberg => ("https://codeberg.org/api/v1/repos", "limit", None),
    };

    let url = format!("{api}/{name}/releases/latest");
    match fetch_json::<ReleaseResponse>(&url, "release", token) {
        Ok(response) => Ok(response.release()),
        Err(e) if status(&e) == Some(404) => {
            let url = format!("{api}/{name}/releases?{page_param}={FALLBACK_PAGE}");
            let listed: Vec<ReleaseResponse> =
                fetch_json(&url, "release", token).map_err(|e| with_quota_hint(host, token, e))?;
            newest(listed)
                .map(ReleaseResponse::release)
                .ok_or_else(|| anyhow::anyhow!("{url} lists no non-draft release"))
        }
        Err(e) => Err(with_quota_hint(host, token, e)),
    }
}

/// The list endpoint answers newest-first and keeps drafts in; `/releases/latest`
/// never returns a draft, so neither does the fallback.
fn newest(releases: Vec<ReleaseResponse>) -> Option<ReleaseResponse> {
    releases.into_iter().find(|release| !release.draft)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "hits the real GitHub API; run with `cargo test -- --ignored`"]
    fn fetches_real_delta_release() {
        let release = latest_release("dandavison/delta", Host::Github)
            .expect("dandavison/delta should have a latest release");
        assert!(!release.tag.is_empty());
        assert!(!release.assets.is_empty());
    }

    #[test]
    fn newest_skips_drafts() {
        let listed: Vec<ReleaseResponse> = serde_json::from_str(
            r#"[
                {"tag_name": "v0.2.0", "draft": true, "assets": []},
                {"tag_name": "v0.1.0", "draft": false, "assets": []}
            ]"#,
        )
        .expect("fixture matches ReleaseResponse");
        let picked = newest(listed).expect("one published release");
        assert_eq!(picked.tag_name, "v0.1.0");
    }

    #[test]
    fn newest_is_none_when_every_entry_is_a_draft() {
        let listed: Vec<ReleaseResponse> =
            serde_json::from_str(r#"[{"tag_name": "v0.1.0", "draft": true, "assets": []}]"#)
                .expect("fixture matches ReleaseResponse");
        assert!(newest(listed).is_none());
    }

    #[test]
    fn status_reads_the_code_out_of_the_error_chain() {
        let err =
            anyhow::Error::new(ureq::Error::StatusCode(404)).context("failed to query a release");
        assert_eq!(status(&err), Some(404));
        assert_eq!(status(&anyhow::anyhow!("no status behind this")), None);
    }

    #[test]
    #[ignore = "hits the real GitHub API; run with `cargo test -- --ignored`"]
    fn falls_back_when_every_release_is_a_prerelease() {
        let release = latest_release("osenvosem/tickview", Host::Github)
            .expect("tickview publishes prerelease-only releases");
        assert!(!release.assets.is_empty());
    }
}
