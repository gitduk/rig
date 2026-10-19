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
    assets: Vec<AssetResponse>,
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

/// GitHub answers an exhausted quota with 403 (primary) or 429 (secondary).
fn is_forbidden(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<ureq::Error>(),
            Some(ureq::Error::StatusCode(403 | 429))
        )
    })
}

/// GitHub and Codeberg (Gitea) release APIs share this response shape.
pub fn latest_release(name: &str, host: Host) -> anyhow::Result<Release> {
    let (url, token) = match host {
        Host::Github => (
            format!("https://api.github.com/repos/{name}/releases/latest"),
            github_token(),
        ),
        Host::Codeberg => (
            format!("https://codeberg.org/api/v1/repos/{name}/releases/latest"),
            None,
        ),
    };

    let resp: ReleaseResponse = fetch_json(&url, "release", token).map_err(|e| {
        if host == Host::Github && token.is_none() && is_forbidden(&e) {
            e.context(
                "GitHub API rate limit likely exhausted (60 requests/hour unauthenticated) \
                 — set GITHUB_TOKEN or run `gh auth login`",
            )
        } else {
            e
        }
    })?;

    Ok(Release {
        tag: resp.tag_name,
        assets: resp
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
                size: a.size,
                digest: a.digest,
            })
            .collect(),
    })
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
}
