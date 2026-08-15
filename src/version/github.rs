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

/// GitHub and Codeberg (Gitea) release APIs share this response shape.
pub fn latest_release(name: &str, host: Host) -> anyhow::Result<Release> {
    let url = match host {
        Host::Github => format!("https://api.github.com/repos/{name}/releases/latest"),
        Host::Codeberg => format!("https://codeberg.org/api/v1/repos/{name}/releases/latest"),
    };

    let resp: ReleaseResponse = fetch_json(&url, "release")?;

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
