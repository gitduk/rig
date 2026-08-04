use serde::Deserialize;

use super::fetch_json;

#[derive(Deserialize)]
struct CratesIoResponse {
    #[serde(rename = "crate")]
    krate: CrateInfo,
}

#[derive(Deserialize)]
struct CrateInfo {
    max_stable_version: String,
}

/// Version comparison for `[[cargo]]`, since `cargo install --force` has
/// no version diff of its own.
pub fn latest_version(name: &str) -> anyhow::Result<String> {
    let url = format!("https://crates.io/api/v1/crates/{name}");

    let resp: CratesIoResponse = fetch_json(&url, "crates.io")?;

    Ok(resp.krate.max_stable_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "hits the real crates.io API; run with `cargo test -- --ignored`"]
    fn fetches_real_serde_version() {
        let version = latest_version("serde").expect("serde should have a max_stable_version");
        assert!(!version.is_empty());
    }
}
