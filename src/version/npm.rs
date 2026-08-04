use std::collections::HashMap;

use serde::Deserialize;

use super::fetch_json;

pub struct NpmInfo {
    pub version: String,
    pub bin: HashMap<String, String>,
}

#[derive(Deserialize)]
struct NpmLatestResponse {
    version: String,
    #[serde(default)]
    bin: HashMap<String, String>,
}

/// Scoped package names (`@scope/name`) need the `/` percent-encoded to stay
/// a single path segment against the registry's `/{package}/{version}` route.
fn registry_path(pkg: &str) -> String {
    match pkg.strip_prefix('@').and_then(|rest| rest.split_once('/')) {
        Some((scope, name)) => format!("@{scope}%2f{name}"),
        None => pkg.to_string(),
    }
}

/// Version query bypasses the package manager entirely, so it works the
/// same whether bun or npm ends up doing the actual install.
pub fn latest(pkg: &str) -> anyhow::Result<NpmInfo> {
    let url = format!("https://registry.npmjs.org/{}/latest", registry_path(pkg));

    let resp: NpmLatestResponse = fetch_json(&url, "npm registry")?;

    Ok(NpmInfo {
        version: resp.version,
        bin: resp.bin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_scoped_package_names() {
        assert_eq!(registry_path("@openai/codex"), "@openai%2fcodex");
    }

    #[test]
    fn leaves_unscoped_package_names_alone() {
        assert_eq!(registry_path("navi"), "navi");
    }

    #[test]
    #[ignore = "hits the real npm registry; run with `cargo test -- --ignored`"]
    fn fetches_real_scoped_package() {
        let info = latest("@openai/codex").expect("@openai/codex should resolve");
        assert!(!info.version.is_empty());
        assert!(info.bin.contains_key("codex"));
    }
}
